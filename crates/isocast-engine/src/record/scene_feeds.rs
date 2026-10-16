//! Scenes drawn for monitors whether or not they are on air (`scene:ID`
//! targets): a scene projector and the multiview's scene tiles (G.4,
//! #19). One thread of its own, with a compositor of its own (as the
//! virtual camera's scene target has), draws each scene someone watches
//! into that scene's frame slot at the highest rate any viewer asks for,
//! and stops once nobody has asked for [`HOLD`]. It never runs on the
//! program's thread or touches its compositor, so the program and the
//! recordings don't wait for it; it only shares the GPU.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::OnAir;
use super::program::{Compositor, Feed};
use super::scenes::ProgramScenes;
use crate::gpu::Gpu;
use crate::source::{FrameSlot, Timeline};
use crate::{Platform, Time};

/// How long a scene keeps being drawn after a viewer last asked for it.
const HOLD: Duration = Duration::from_secs(3);
/// The most frames a second a scene is drawn at.
const MAX_FPS: u32 = 60;

/// The prefix of a scene target.
pub(crate) const PREFIX: &str = "scene:";

/// The scenes being watched, and whether the thread drawing them runs.
pub(super) struct SceneFeeds<F> {
    state: Mutex<State<F>>,
    timeline: Arc<Timeline>,
}

struct State<F> {
    feeds: HashMap<Arc<str>, Watched<F>>,
    /// The drawing thread runs (it ends, under this lock, once nothing is
    /// watched).
    running: bool,
}

/// A watched scene: its id, where its frames go, and the rate asked for.
type Watching<F> = (Arc<str>, Arc<FrameSlot<F>>, u32);

struct Watched<F> {
    slot: Arc<FrameSlot<F>>,
    /// The highest rate asked for since it was last drawn, and when last
    /// asked.
    fps: u32,
    asked: Instant,
}

impl<F: Clone> SceneFeeds<F> {
    pub(super) fn new(timeline: Arc<Timeline>) -> Self {
        Self {
            state: Mutex::new(State {
                feeds: HashMap::new(),
                running: false,
            }),
            timeline,
        }
    }

    /// Someone shows scene `id` at `fps`: where its frames arrive, and
    /// whether the drawing thread must be started (the caller starts it).
    pub(super) fn want(&self, id: &str, fps: u32) -> (Arc<FrameSlot<F>>, bool) {
        let mut state = self.state.lock();
        let now = Instant::now();
        let watched = state.feeds.entry(id.into()).or_insert_with(|| Watched {
            slot: Arc::new(FrameSlot::with_capacity(self.timeline.clone(), 1)),
            fps: 0,
            asked: now,
        });
        watched.fps = watched.fps.max(fps.clamp(1, MAX_FPS));
        watched.asked = now;
        let slot = watched.slot.clone();
        let start = !std::mem::replace(&mut state.running, true);
        (slot, start)
    }

    /// The scenes still watched and their rates (those nobody asked for
    /// within [`HOLD`] are let go), or none: the thread is then marked
    /// stopped and must end.
    fn watched(&self) -> Option<Vec<Watching<F>>> {
        let mut state = self.state.lock();
        let now = Instant::now();
        state
            .feeds
            .retain(|_, watched| now.duration_since(watched.asked) < HOLD);
        if state.feeds.is_empty() {
            state.running = false;
            return None;
        }
        Some(
            state
                .feeds
                .iter()
                .map(|(id, watched)| (id.clone(), watched.slot.clone(), watched.fps))
                .collect(),
        )
    }

    /// The thread is gone (it failed or the engine stops): start a new one
    /// at the next ask.
    pub(super) fn stopped(&self) {
        self.state.lock().running = false;
    }
}

/// Draw the watched scenes until none is watched or `stop` says so.
/// `scenes` gives the setup's scenes as the program last compiled them;
/// `epoch` is the engine clock's zero (animations count from it). Each
/// scene has a compositor of its own (so its animations and what it
/// shows start as when it comes on air), all on one GPU device.
pub(super) fn run<P: Platform>(
    platform: &P,
    feeds: &SceneFeeds<P::Frame>,
    scenes: impl Fn() -> Option<Arc<ProgramScenes<P::Frame>>>,
    epoch: Time,
    stop: impl Fn() -> bool,
) -> anyhow::Result<()> {
    let gpu = Gpu::new()?;
    // Each watched scene's compositor, the scenes version it adopted, and
    // when the scene is next due.
    let mut drawing: HashMap<Arc<str>, (Compositor<P>, u64, Instant)> = HashMap::new();
    loop {
        if stop() {
            feeds.stopped();
            return Ok(());
        }
        let Some(watched) = feeds.watched() else {
            return Ok(());
        };
        drawing.retain(|id, _| watched.iter().any(|(watched, _, _)| watched == id));
        let now = Instant::now();
        let next = watched
            .iter()
            .map(|(id, _, _)| drawing.get(id).map_or(now, |(_, _, due)| *due))
            .min()
            .unwrap_or(now);
        if next > now {
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
            continue;
        }
        let Some(latest) = scenes() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let tick = platform.now();
        let canvas = latest.compiled.canvas;
        for (id, slot, fps) in &watched {
            let stale = drawing
                .get(id)
                .is_none_or(|(compositor, _, _)| compositor.canvas() != canvas);
            if stale {
                let mut made = Compositor::with_gpu(platform, canvas, gpu.clone())?;
                made.set_epoch(epoch);
                drawing.insert(id.clone(), (made, u64::MAX, now));
            }
            let (compositor, version, due) = drawing.get_mut(id).expect("made above");
            if *due > now {
                continue;
            }
            // On its own grid, skipping what was missed rather than
            // catching up.
            let period = Duration::from_secs(1) / (*fps).max(1);
            *due = (*due + period).max(now);
            if *version != latest.version {
                *version = latest.version;
                compositor.adopt(latest.clone(), tick);
            }
            let scene = OnAir {
                scene: id.clone(),
                from: None,
            };
            if let Some((frame, _)) = compositor.draw(Feed::Preview, Some(&scene), tick, tick)? {
                slot.push(frame, tick);
            }
        }
    }
}
