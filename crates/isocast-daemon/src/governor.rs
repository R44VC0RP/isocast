//! One sampler/decision owner, outside media callbacks and without disk I/O.
use crate::{Daemon, streams::Output};
#[cfg(target_os = "macos")]
use isocast_engine::Platform;
use isocast_engine::governor::{Controller, Sample};
use isocast_protocol::{CanvasId, PROGRAM, VERTICAL_PROGRAM, api::Event, stream::StreamState};
use std::collections::HashMap;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Previous {
    session: Option<u64>,
    record_frames: u64,
    main: (bool, u64, u64),
    vertical: (bool, u64, u64),
    streams: HashMap<String, (Option<String>, u64)>,
    stalls: u64,
    progressed: Option<Instant>,
}

/// Cheap lifecycle/pressure probe. No setup, status strings, engine stats or
/// file lists are cloned until something changes or there is actual pressure.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Activity {
    revision: u64,
    enabled: bool,
    session: Option<u64>,
    recording: bool,
    paused: bool,
    streams: [[u32; 3]; 2],
    replay: [bool; 2],
}

impl Activity {
    fn active(self) -> bool {
        self.recording
            || self.streams.iter().flatten().any(|count| *count > 0)
            || self.replay.iter().any(|running| *running)
    }
}

fn activity(daemon: &Daemon) -> (Activity, bool) {
    let state = daemon.state.read();
    let mut activity = Activity {
        revision: state.revision,
        enabled: state
            .setup
            .as_ref()
            .is_none_or(|setup| setup.governor.enabled),
        session: state.session.as_ref().map(|session| session.id),
        recording: state
            .session
            .as_ref()
            .is_some_and(|session| session.files.lock().is_some()),
        paused: false,
        streams: [[0; 3]; 2],
        replay: [
            state.replay.active_on(CanvasId::Main),
            state.replay.active_on(CanvasId::Vertical),
        ],
    };
    let mut pressure = false;
    if let Some(session) = &state.session
        && let Some(live) = session.live.lock().as_ref()
    {
        activity.paused = live.paused;
        pressure = live.disk.as_ref().is_some_and(|disk| disk.behind());
    }
    for (id, output) in &state.streams {
        let Output::Running(stream) = output else {
            continue;
        };
        let stream_state = stream.governor_state();
        let index = match stream_state {
            StreamState::Connecting => 0,
            StreamState::Live => 1,
            StreamState::Reconnecting => 2,
            _ => continue,
        };
        activity.streams[usize::from(id.starts_with("vertical:"))][index] += 1;
        if activity.enabled && stream_state == StreamState::Live {
            pressure |= stream.governor_congestion() >= 0.25;
        }
    }
    (activity, pressure)
}

pub(super) fn start(daemon: &Arc<Daemon>) {
    let weak = Arc::downgrade(daemon);
    let failed = weak.clone();
    if let Err(error) = isocast_diagnostics::spawn(
        "output headroom",
        move || {
            #[cfg(target_os = "macos")]
            let mut monitor = isocast_platform_macos::pressure::LoadMonitor::default();
            let mut controller = Controller::default();
            let mut previous = Previous::default();
            let mut last_activity: Option<Activity> = None;
            loop {
                let Some(daemon) = weak.upgrade() else { break };
                if daemon.shutting_down.load(Ordering::SeqCst) {
                    break;
                }
                let (activity, mut pressure) = activity(&daemon);
                let active = activity.active();
                if active {
                    controller.observe_monitors();
                    // Keep #90 alive even with automatic policy disabled.
                    #[cfg(target_os = "macos")]
                    {
                        if let Some(warning) = monitor.poll() {
                            daemon.publish(Event::Warning {
                                message: warning.message,
                            });
                        }
                        pressure |= crate::platform::NativePlatform
                            .governor_headroom()
                            .is_some_and(|sample| sample.encoder_threatened);
                    }
                } else if last_activity.is_some_and(Activity::active) {
                    // Clear stale native warning/health once, not on idle ticks.
                    #[cfg(target_os = "macos")]
                    {
                        monitor = Default::default();
                    }
                }
                let native = isocast_engine::record::pressure::snapshot();
                pressure |= native.0 > 0 || native.2 != previous.stalls;
                if last_activity != Some(activity)
                    || controller.needs_poll()
                    || active && activity.enabled && pressure
                {
                    let (settings, sample) = sample(&daemon, &mut previous);
                    if let Some(governor) = controller.tick(&settings, &sample) {
                        daemon.publish(Event::GovernorChanged { governor });
                    }
                }
                last_activity = Some(activity);
                drop(daemon);
                std::thread::sleep(Duration::from_millis(100));
            }
        },
        move |_| {
            if let Some(daemon) = failed.upgrade() {
                daemon.publish(Event::Warning { message: "Automatic quality monitoring stopped; native overload protection remains active. Restart the engine after recording".into() });
            }
        },
    ) {
        daemon.publish(Event::Warning {
            message: format!("output headroom monitoring unavailable: {error}"),
        });
    }
}

fn sample(
    daemon: &Daemon,
    previous: &mut Previous,
) -> (isocast_protocol::governor::GovernorSettings, Sample) {
    let (setup, engine, session, ready, live, streams, replay, monitors) = {
        let state = daemon.state.read();
        let streams = state
            .streams
            .iter()
            .filter_map(|(id, output)| {
                let Output::Running(stream) = output else {
                    return None;
                };
                let status = stream.status();
                status
                    .is_active()
                    .then(|| (id.clone(), status, stream.governor_congestion()))
            })
            .collect::<Vec<_>>();
        (
            state.setup.clone(),
            state.engine.clone(),
            state.session.as_ref().map(|s| s.id),
            state
                .session
                .as_ref()
                .is_some_and(|s| s.files.lock().is_some()),
            state.session.as_ref().and_then(|s| s.live.lock().clone()),
            streams,
            [
                state.replay.active_on(CanvasId::Main),
                state.replay.active_on(CanvasId::Vertical),
            ],
            state
                .wanted_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(3)),
        )
    };
    let settings = setup
        .as_ref()
        .map(|s| s.governor.clone())
        .unwrap_or_default();
    let mut sample = Sample {
        has_monitors: monitors,
        ..Default::default()
    };
    let (pressure, iso, stalls) = isocast_engine::record::pressure::snapshot();
    sample.encoder_pressure = pressure > 0 || stalls != previous.stalls;
    sample.native_iso_percent = iso;
    previous.stalls = stalls;
    #[cfg(target_os = "macos")]
    {
        sample.headroom = crate::platform::NativePlatform.governor_headroom();
    }
    let pause = engine
        .as_ref()
        .and_then(|engine| engine.control().pause_status(crate::now()));
    let paused = pause.is_some_and(|p| p.paused);
    let recording = ready && !paused;
    // A take keeps its original files when the editable setup changes.
    // Use those files, not a newly disabled program/ISO checkbox.
    sample.main_protected = recording
        && live.as_ref().map_or_else(
            || setup.as_ref().is_some_and(|s| s.recording.program.enabled),
            |live| live.files.iter().any(|f| f.source == PROGRAM),
        );
    sample.vertical_protected = recording
        && live.as_ref().map_or_else(
            || {
                setup
                    .as_ref()
                    .and_then(|s| s.vertical.as_ref())
                    .and_then(|v| v.recording.as_ref())
                    .is_some_and(|p| p.enabled)
            },
            |live| live.files.iter().any(|f| f.source == VERTICAL_PROGRAM),
        );
    sample.has_isos = recording
        && live.as_ref().map_or_else(
            || {
                setup
                    .as_ref()
                    .is_some_and(|s| s.recording.iso.enabled && !s.recording.iso.sources.is_empty())
            },
            |live| {
                live.files
                    .iter()
                    .any(|f| f.source != PROGRAM && f.source != VERTICAL_PROGRAM)
            },
        );
    let mut progressed = false;
    sample.has_stream = !streams.is_empty();
    let mut live_stream = false;
    previous
        .streams
        .retain(|id, _| streams.iter().any(|(key, ..)| key == id));
    for (id, status, congestion) in &streams {
        let vertical = id.starts_with("vertical:");
        let canvas_bit = if vertical { 2 } else { 1 };
        sample.stream_canvases |= canvas_bit;
        // Connecting is not upload saturation; don't reduce a normal cold start.
        if status.state == StreamState::Live {
            live_stream = true;
            sample.upload_congestion = sample.upload_congestion.max(*congestion);
            if *congestion >= 0.25 {
                sample.congested_canvases |= canvas_bit;
            }
            if vertical {
                sample.vertical_protected = true;
            } else {
                sample.main_protected = true;
            }
        }
        if let Some((since, frames)) = previous.streams.get(id)
            && *since == status.since
        {
            progressed |= status.frames_sent > *frames;
        }
        previous
            .streams
            .insert(id.clone(), (status.since.clone(), status.frames_sent));
    }
    // Replay shares a protected main encoder. Its own sink reports native
    // loss through the shared encoder worker's immediate loss hook.
    sample.main_protected |= replay[0];
    sample.vertical_protected |= replay[1];
    sample.active = ready || sample.has_stream || replay.iter().any(|on| *on);
    sample.paused = (!ready || paused) && !live_stream && !replay.iter().any(|on| *on);
    sample.has_vertical = sample.vertical_protected;
    if let Some(live) = &live {
        let frames = live.files.iter().map(|f| f.frames).sum::<u64>();
        if previous.session == session && !paused {
            progressed |= frames > previous.record_frames;
        }
        previous.record_frames = frames;
        // Live dropped-frame totals include delayed, deliberately refused
        // vertical pictures. Never reinterpret those totals as new loss on
        // restoration/pinning or a take reset. File/encoder/route producers
        // publish new unintended losses with owner-local admission provenance.
        sample.disk_behind = live.disk.as_ref().and_then(|disk| {
            disk.measured_at_ms
                .filter(|at| isocast_engine::governor::clock_ms().saturating_sub(*at) < 5000)
                .map(|_| disk.behind())
        });
    } else {
        sample.disk_behind = Some(false);
    }
    previous.session = session;
    if let Some(engine) = engine.filter(|_| sample.active) {
        let info = engine.info();
        let main = (
            sample.main_protected,
            info.program_frames,
            info.missed_frames,
        );
        let vertical = info
            .vertical
            .as_ref()
            .map(|v| (sample.vertical_protected, v.program_frames, v.missed_frames))
            .unwrap_or_default();
        for (current, last) in [
            (main, &mut previous.main),
            (vertical, &mut previous.vertical),
        ] {
            if current.0 && last.0 {
                sample.protected_loss |= current.2 > last.2;
                progressed |= current.1 > last.1;
            }
            *last = current;
        }
        let budget = 1_000_000.0 / f64::from(info.canvas.fps.max(1));
        sample.gpu_late = sample.protected_loss && info.compositor.gpu_wait_us.p99 > budget;
        sample.cpu_late = info.clocks.iter().any(|c| {
            (c.id == PROGRAM && sample.main_protected
                || c.id == VERTICAL_PROGRAM && sample.vertical_protected)
                && c.wake_lateness_us.p99 > c.period_ms * 1500.0
        });
        // Async GPU latency may exceed one frame while throughput still fits.
        sample.gpu_headroom = info.compositor.gpu_wait_us.p99 < budget * 1.4;
    } else {
        sample.gpu_headroom = true;
        previous.main.0 = false;
        previous.vertical.0 = false;
    }
    if progressed {
        previous.progressed = Some(Instant::now());
    }
    let recent = setup
        .as_ref()
        .map_or(1.0, |s| (3.0 / f64::from(s.canvas.fps.max(1))).max(1.5));
    sample.progressed = previous
        .progressed
        .is_some_and(|at| at.elapsed().as_secs_f64() < recent);
    (settings, sample)
}
