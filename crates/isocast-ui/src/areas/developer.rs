//! Developer mode (#47; ui/areas/developer/): the Developer menu, shown
//! while the live setup's profile has developer mode on, and the live stats
//! overlay. The setting itself is the profile's (Settings › Advanced, or
//! `isocast debug on`); this window follows it, and turns its own process's
//! developer logs on and off with it.
//!
//! The overlay reads `debug_status` (the engine's latest per-second trace:
//! outputs, compositor, pacing) and the window's status once a second, only
//! while it is shown; nothing here runs while developer mode is off.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use isocast_protocol::api::Method;
use isocast_protocol::developer::DeveloperStatus;
use serde_json::Value;
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::usage::Meter;
use crate::{AppWindow, DeveloperArea, OutputStat, StatRow, ToastKind};

/// Snapshot states (`--state`): the overlay with what the engine measures
/// now, or with a fixed sample (for pictures that don't change).
pub const STATES: &[&str] = &["developer-overlay", "developer-overlay-sample"];

/// The feed released apps read (scripts/release.sh `feed_url`), shown when
/// this build has none in its Info.plist (a development build).
const RELEASE_FEED: &str =
    "https://github.com/R44VC0RP/isocast/releases/latest/download/appcast.xml";

#[derive(Default)]
pub struct Developer {
    /// Developer mode as last applied to this process.
    on: Cell<bool>,
    /// Reads the overlay's numbers once a second while it is shown.
    timer: slint::Timer,
    /// A `debug_status` call is out (one at a time).
    asking: Cell<bool>,
    /// The window's own CPU since the last reading.
    meter: RefCell<Option<Meter>>,
    /// The engine's latest answer, for Copy Diagnostics.
    last: RefCell<Option<DeveloperStatus>>,
}

impl Area for Developer {
    fn connect(&self, _app: &Rc<App>, _window: &AppWindow) {}

    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed) {
        if !changed.any(Changed::SETUP) {
            return;
        }
        let on = app
            .session
            .borrow()
            .setup
            .as_ref()
            .is_some_and(|s| s.developer.enabled);
        if on == self.on.get() {
            return;
        }
        self.on.set(on);
        isocast_diagnostics::set_developer(on);
        let area = window.global::<DeveloperArea>();
        area.set_on(on);
        if on {
            isocast_diagnostics::debug!("developer mode on in this window");
        } else {
            self.show_overlay(app, false);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "dev_stats_overlay" => {
                let shown = app
                    .window()
                    .is_some_and(|w| w.global::<DeveloperArea>().get_overlay());
                self.show_overlay(app, !shown);
            }
            "dev_open_logs" => {
                crate::platform::open_folder(&isocast_diagnostics::logs_folder().to_string_lossy())
            }
            "dev_copy_diagnostics" => self.copy_diagnostics(app),
            "dev_update_feed" => {
                let feed = crate::update::feed_url().unwrap_or_else(|| RELEASE_FEED.into());
                crate::platform::open_url(&feed);
            }
            "dev_restart_engine" => {
                // isocastd refuses (atomically) while anything runs; the
                // window's link starts it again.
                app.call(Method::ShutdownIfIdle, |app, result| match result {
                    Ok(_) => {
                        app.toast(ToastKind::Info, "Restarting the engine", "");
                    }
                    Err(error) => app.failed("The engine wasn't restarted", &error),
                });
            }
            _ => return false,
        }
        true
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        _arg: &str,
    ) -> Option<anyhow::Result<()>> {
        match name {
            "developer-overlay" => {
                self.show_overlay(app, true);
                self.refresh(app);
            }
            "developer-overlay-sample" => {
                let area = window.global::<DeveloperArea>();
                area.set_on(true);
                area.set_overlay(true);
                self.show(app, &sample());
            }
            _ => return None,
        }
        Some(Ok(()))
    }
}

impl Developer {
    fn show_overlay(&self, app: &Rc<App>, shown: bool) {
        let Some(window) = app.window() else { return };
        window.global::<DeveloperArea>().set_overlay(shown);
        if !shown {
            self.timer.stop();
            *self.meter.borrow_mut() = None;
            return;
        }
        *self.meter.borrow_mut() = Some(Meter::start());
        self.refresh(app);
        let weak = Rc::downgrade(app);
        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(app) = weak.upgrade() {
                    app.developer.refresh(&app);
                }
            },
        );
    }

    /// Ask the engine for its latest trace and show it.
    fn refresh(&self, app: &Rc<App>) {
        if self.asking.replace(true) {
            return;
        }
        app.call_as::<DeveloperStatus>(Method::DebugStatus, |app, result| {
            app.developer.asking.set(false);
            match result {
                Ok(status) => {
                    app.developer.show(app, &status);
                    *app.developer.last.borrow_mut() = Some(status);
                }
                Err(error) => {
                    if let Some(window) = app.window() {
                        let area = window.global::<DeveloperArea>();
                        area.set_note(format!("No engine: {}", error.message).into());
                        area.set_rows(ModelRc::default());
                        area.set_outputs(ModelRc::default());
                    }
                }
            }
        });
    }

    fn show(&self, app: &Rc<App>, status: &DeveloperStatus) {
        let Some(window) = app.window() else { return };
        let area = window.global::<DeveloperArea>();
        let engine_cpu = app
            .session
            .borrow()
            .status
            .as_ref()
            .and_then(|s| s.cpu_percent);
        let app_cpu = self.meter.borrow_mut().as_mut().map(|m| m.percent());
        let (rows, outputs, note) = overlay(status, engine_cpu, app_cpu, memory_bytes());
        area.set_rows(ModelRc::new(VecModel::from(rows)));
        area.set_outputs(ModelRc::new(VecModel::from(outputs)));
        area.set_note(note.into());
    }

    /// Developer › Copy Diagnostics: a short summary, redacted like the
    /// logs, for pasting into an issue or a chat.
    fn copy_diagnostics(&self, app: &Rc<App>) {
        app.call_as::<DeveloperStatus>(Method::DebugStatus, |app, result| {
            let status = match result {
                Ok(status) => Some(status),
                Err(_) => app.developer.last.borrow().clone(),
            };
            let s = app.session.borrow();
            let engine = s.status.as_ref();
            let mut text = format!(
                "Isocast {} ({})\nengine: {:?}, cpu {}\n",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::ARCH,
                engine.map(|e| e.state),
                engine
                    .and_then(|e| e.cpu_percent)
                    .map_or("n/a".into(), |c| format!("{c:.1}%")),
            );
            if let Some(engine) = engine.and_then(|e| e.engine.as_ref()) {
                text.push_str(&format!(
                    "program: {:.1} fps, {} frames, {} missed, composite p50/p99 {:.0}/{:.0} µs\n",
                    engine.fps,
                    engine.program_frames,
                    engine.missed_frames,
                    engine.compositor.draw_wall_us.p50,
                    engine.compositor.draw_wall_us.p99
                ));
            }
            if let Some(live) = engine.and_then(|e| e.live.as_ref()) {
                for file in &live.files {
                    text.push_str(&format!(
                        "file {}: {} frames, {} dropped, {} audio dropouts\n",
                        file.source, file.frames, file.dropped_frames, file.audio_dropouts
                    ));
                }
            }
            drop(s);
            if let Some(status) = &status {
                text.push_str(&format!(
                    "developer mode: {} (since {}, auto-off {})\nlogs: {} ({:.1} of {} MiB)\n",
                    if status.active { "on" } else { "off" },
                    status.since.as_deref().unwrap_or("-"),
                    status.auto_off_at.as_deref().unwrap_or("never"),
                    status.logs_folder.display(),
                    status.log_bytes as f64 / (1u64 << 20) as f64,
                    status.log_budget_bytes >> 20
                ));
                if let Some(trace) = &status.trace {
                    text.push_str(&format!("last trace: {trace}\n"));
                }
            }
            let text = isocast_diagnostics::redact::Redactor::default().text(&text);
            crate::platform::copy_text(&text);
            app.toast(
                ToastKind::Success,
                "Copied diagnostics",
                "A redacted summary is on the clipboard.",
            );
        });
    }
}

/// The overlay's lines and output rows from the engine's latest trace.
fn overlay(
    status: &DeveloperStatus,
    engine_cpu: Option<f64>,
    app_cpu: Option<f64>,
    app_memory: Option<u64>,
) -> (Vec<StatRow>, Vec<OutputStat>, String) {
    let row = |label: &str, value: String, short: String, warn: bool| StatRow {
        label: label.into(),
        value: value.into(),
        short: short.into(),
        warn,
    };
    let mb =
        |bytes: Option<u64>| bytes.map_or("–".into(), |b| format!("{:.0} MB", b as f64 / 1e6));
    let pct = |cpu: Option<f64>| cpu.map_or("–".into(), |c| format!("{c:.1}%"));
    let mut rows = Vec::new();
    let mut outputs = Vec::new();
    let mut note = String::new();
    match &status.trace {
        Some(trace) => {
            let program = &trace["program"];
            let us = |v: &Value| v.as_f64().unwrap_or(0.0) / 1000.0;
            let fps = format!(
                "{:.1} fps · {} missed",
                program["fps"].as_f64().unwrap_or(0.0),
                program["missed"].as_u64().unwrap_or(0)
            );
            rows.push(row(
                "Program",
                fps.clone(),
                fps,
                program["missed"].as_u64().unwrap_or(0) > 0,
            ));
            let (p50, p99) = (
                us(&program["composite_wall_us"]["p50"]),
                us(&program["composite_wall_us"]["p99"]),
            );
            rows.push(row(
                "Frame time",
                format!("{p50:.2} / {p99:.2} ms (p50/p99)"),
                format!("frame {p50:.2}/{p99:.2} ms"),
                false,
            ));
            let late: u64 = trace["pacing"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| c["late_wakes"].as_u64().unwrap_or(0))
                .sum();
            let worst = trace["pacing"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| c["wake_lateness_us"]["p99"].as_f64().unwrap_or(0.0))
                .fold(0.0, f64::max);
            // Late wakes first and short enough not to elide (#55): the
            // clocks' p99 wake lateness after.
            rows.push(row(
                "Pacing",
                format!("{late} late · p99 {:.2} ms", worst / 1000.0),
                format!("{late} late · {:.2} ms", worst / 1000.0),
                late > 0,
            ));
            let queues: Vec<String> = trace["stream_encoders"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    format!(
                        "{} {}",
                        e["purpose"].as_str().unwrap_or("?"),
                        e["in_flight"].as_u64().unwrap_or(0)
                    )
                })
                .collect();
            if !queues.is_empty() {
                let queues = queues.join(" · ");
                rows.push(row(
                    "Encoder queues",
                    queues.clone(),
                    format!("enc {queues}"),
                    false,
                ));
            }
            for output in trace["outputs"].as_array().into_iter().flatten() {
                let dropped = output["dropped"].as_u64().unwrap_or(0);
                let late = output["late"].as_u64().unwrap_or(0);
                outputs.push(OutputStat {
                    name: output["source"].as_str().unwrap_or("?").into(),
                    fps: output["frames"].to_string().into(),
                    dropped: dropped.to_string().into(),
                    late: late.to_string().into(),
                    submit: format!(
                        "{:.2}",
                        output["submit_us"]["mean"].as_f64().unwrap_or(0.0) / 1000.0
                    )
                    .into(),
                    queue: output["writer_queue_max"].to_string().into(),
                    warn: dropped > 0 || late > 0,
                });
            }
        }
        None if status.active => note = "Engine idle: nothing to measure".into(),
        None => note = "Developer mode is starting".into(),
    }
    let engine = format!("{} · {}", pct(engine_cpu), mb(status.memory_bytes));
    rows.push(row(
        "Engine",
        engine.clone(),
        format!("eng {engine}"),
        false,
    ));
    let window = format!("{} · {}", pct(app_cpu), mb(app_memory));
    rows.push(row("App", window.clone(), format!("app {window}"), false));
    (rows, outputs, note)
}

/// This process's memory as Activity Monitor shows it (its footprint).
fn memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut value = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
        // libproc writes flavor 4's public structure.
        let result = unsafe {
            libc::proc_pid_rusage(std::process::id() as i32, 4, value.as_mut_ptr().cast())
        };
        (result == 0).then(|| unsafe { value.assume_init() }.ri_phys_footprint)
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// A fixed trace for snapshots: a 4K60 program and three ISO files, one
/// of them dropping.
fn sample() -> DeveloperStatus {
    let trace = serde_json::json!({
        "program": {"fps": 59.9, "missed": 0, "composite_wall_us": {"p50": 1412.0, "p99": 3920.0}},
        "pacing": [{"late_wakes": 0, "wake_lateness_us": {"p99": 610.0}}],
        "stream_encoders": [{"purpose": "monitor", "in_flight": 1}],
        "outputs": [
            {"source": "program", "frames": 60, "dropped": 0, "late": 0, "submit_us": {"mean": 412.0}, "writer_queue_max": 1},
            {"source": "desktop", "frames": 60, "dropped": 0, "late": 0, "submit_us": {"mean": 388.0}, "writer_queue_max": 0},
            {"source": "camera", "frames": 60, "dropped": 0, "late": 0, "submit_us": {"mean": 205.0}, "writer_queue_max": 0},
            {"source": "slides", "frames": 57, "dropped": 3, "late": 2, "submit_us": {"mean": 1830.0}, "writer_queue_max": 4},
        ],
    });
    DeveloperStatus {
        enabled: true,
        active: true,
        auto_off_hours: 24,
        since: None,
        auto_off_at: None,
        profile: None,
        logs_folder: Default::default(),
        log_bytes: 0,
        log_budget_bytes: isocast_diagnostics::DEVELOPER_BUDGET,
        memory_bytes: Some(412_000_000),
        trace: Some(trace),
    }
}
