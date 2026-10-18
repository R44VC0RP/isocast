//! The live setup's canvases (#84): `canvas_add`, `canvas_set` and
//! `canvas_remove`. A canvas besides the main one is `[vertical]` (id
//! `vertical`) or a `[[canvases]]` entry; each change is an `apply` of the
//! whole setup, so its problems come back with their field paths.

use isocast_protocol::api::{ApplyParams, CANVAS_CLEARABLE, CanvasParams, Canvases, RpcError};
use isocast_protocol::change::Change;
use isocast_protocol::{CanvasId, Setup, VerticalCanvas, VerticalRecording};

use crate::Daemon;

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

fn wrong(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

/// Apply `params`' fields to `canvas`.
fn set_fields(canvas: &mut VerticalCanvas, params: &CanvasParams) -> Result<(), RpcError> {
    for field in &params.clear {
        match field.as_str() {
            "name" => canvas.name = None,
            "fps" => canvas.fps = None,
            "stream" => canvas.stream = None,
            other => {
                return Err(invalid(format!(
                    "clear: {other:?} can't be cleared; fields: {}",
                    CANVAS_CLEARABLE.join(", ")
                )));
            }
        }
    }
    if let Some(name) = &params.name {
        canvas.name = Some(name.clone()).filter(|name| !name.trim().is_empty());
    }
    match (params.width, params.height) {
        (Some(width), Some(height)) => (canvas.width, canvas.height) = (width, height),
        (None, None) => {}
        _ => return Err(invalid("width: give width and height together")),
    }
    if params.fps.is_some() {
        canvas.fps = params.fps;
    }
    if let Some(follow) = params.follow_main {
        canvas.follow_main = follow;
    }
    match params.record {
        Some(true) => {
            let recording = canvas
                .recording
                .get_or_insert_with(VerticalRecording::default);
            recording.enabled = true;
        }
        Some(false) => canvas.recording = None,
        None => {}
    }
    Ok(())
}

impl Daemon {
    pub(crate) fn canvas_add(&self, params: CanvasParams) -> Result<Canvases, RpcError> {
        let canvas = params.canvas;
        if canvas.is_main() {
            return Err(invalid(
                "canvas: the main canvas is always there; change its size with the patch \
                 {\"canvas\": {\"width\": …, \"height\": …}}",
            ));
        }
        let (revision, mut setup) = self.live_canvas_setup()?;
        if setup.has_canvas(canvas) {
            return Err(invalid(format!(
                "canvas: {canvas:?} is taken; change it with `isocast canvas set {canvas}` (or \
                 MCP's canvas_set); canvases: {}",
                ids(&setup)
            )));
        }
        let mut new = if canvas.is_vertical() {
            VerticalCanvas::default()
        } else {
            VerticalCanvas::with_id(canvas.as_str())
        };
        set_fields(&mut new, &params)?;
        if canvas.is_vertical() {
            setup.vertical = Some(new);
        } else {
            if let Some(recording) = new.recording.as_mut() {
                // Its own file name, so it never shares the main one's.
                recording.filename = isocast_protocol::canvas_filename(canvas);
            }
            setup.canvases.push(new);
        }
        self.apply_canvases(revision, setup)
    }

    pub(crate) fn canvas_set(&self, params: CanvasParams) -> Result<Canvases, RpcError> {
        let canvas = params.canvas;
        if canvas.is_main() {
            return Err(invalid(
                "canvas: change the main canvas with the patch {\"canvas\": {…}} (`isocast \
                 apply --patch`, or MCP's apply_change)",
            ));
        }
        let (revision, mut setup) = self.live_canvas_setup()?;
        let known = ids(&setup);
        let Some(entry) = setup.extra_canvas_mut(canvas) else {
            return Err(wrong(format!(
                "canvas: the live setup has no canvas {canvas:?}; add it with `isocast canvas \
                 add {canvas}` (or MCP's canvas_add); canvases: {known}"
            )));
        };
        let had_recording = entry.recording.is_some();
        set_fields(entry, &params)?;
        if !had_recording
            && !canvas.is_vertical()
            && let Some(recording) = entry.recording.as_mut()
        {
            recording.filename = isocast_protocol::canvas_filename(canvas);
        }
        self.apply_canvases(revision, setup)
    }

    pub(crate) fn canvas_remove(&self, canvas: CanvasId) -> Result<Canvases, RpcError> {
        let (revision, mut setup) = self.live_canvas_setup()?;
        if canvas.is_main() || !setup.has_canvas(canvas) {
            return Err(invalid(format!(
                "canvas: no canvas {canvas:?} to remove; canvases: {}",
                ids(&setup)
            )));
        }
        {
            let state = self.state.read();
            if state.streaming_on(canvas) {
                return Err(wrong(format!(
                    "canvas {canvas:?} is streaming; stop it first (`isocast stream stop \
                     --canvas {canvas}`, or MCP's stop_stream with canvas \"{canvas}\")"
                )));
            }
        }
        if canvas.is_vertical() {
            setup.vertical = None;
        } else {
            setup
                .canvases
                .retain(|c| c.id.as_deref() != Some(canvas.as_str()));
        }
        for scene in &mut setup.scenes {
            scene.set_arrangement(canvas, None);
        }
        self.apply_canvases(revision, setup)
    }

    fn live_canvas_setup(&self) -> Result<(u64, Setup), RpcError> {
        let state = self.state.read();
        match &state.setup {
            Some(setup) => Ok((state.revision, setup.clone())),
            None => Err(state.no_setup("no setup to add a canvas to; make one first")),
        }
    }

    fn apply_canvases(&self, revision: u64, setup: Setup) -> Result<Canvases, RpcError> {
        let applied = self.apply(ApplyParams {
            revision,
            change: Change::Setup(Box::new(setup)),
            live: false,
        })?;
        Ok(Canvases {
            revision: applied.revision,
            canvases: crate::canvas_statuses(&self.state.read()),
        })
    }
}

/// The setup's canvas ids, for messages.
fn ids(setup: &Setup) -> String {
    setup
        .canvas_ids()
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
