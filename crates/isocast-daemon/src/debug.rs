//! Developer mode (#47) in isocastd: follows the profile's `[developer]`
//! setting, turns the process-wide gate (`isocast_diagnostics::developer`)
//! on and off, writes one timing trace record a second while the engine
//! runs, and turns itself off after `auto_off_hours`.
//!
//! When it was turned on is kept beside the logs (`developer-mode.json`),
//! so a restarted isocastd keeps counting towards the auto-off instead of
//! starting again.

use parking_lot::Mutex;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use isocast_diagnostics::{developer, emit, emit_developer, set_developer};
use isocast_protocol::api::{ApplyParams, RpcError, Status};
use isocast_protocol::change::Change;
use isocast_protocol::developer::{DebugSetParams, DeveloperSettings, DeveloperStatus};
use serde_json::{Value, json};

use crate::Daemon;

/// When developer mode was turned on, while it is.
static SINCE: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);
/// The latest trace record, for `debug_status` (the stats overlay).
static LAST_TRACE: Mutex<Option<Value>> = Mutex::new(None);

fn since_file() -> PathBuf {
    isocast_diagnostics::logs_folder().join("developer-mode.json")
}

/// Once, when isocastd starts: the host clock for traces, and the loaded
/// profile's setting.
pub(crate) fn start(daemon: &Daemon) {
    isocast_engine::trace::set_clock(crate::now);
    sync(daemon, "loaded");
}

/// The profile's setting.
fn settings(daemon: &Daemon) -> (DeveloperSettings, Option<String>) {
    let state = daemon.state.read();
    (
        state
            .setup
            .as_ref()
            .map(|s| s.developer)
            .unwrap_or_default(),
        state.file.current().map(|c| c.profile.clone()),
    )
}

/// Make the gate follow the profile's setting (after any setup change).
pub(crate) fn sync(daemon: &Daemon, reason: &str) {
    let (settings, profile) = settings(daemon);
    let mut since = SINCE.lock();
    match (settings.enabled, since.is_some()) {
        (true, false) => {
            // A restarted isocastd keeps the time it was turned on.
            let kept = std::fs::read(since_file())
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["since"].as_str().map(String::from))
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&Utc))
                .filter(|t| *t <= Utc::now());
            let at = kept.unwrap_or_else(Utc::now);
            if kept.is_none() {
                write_since(at);
            }
            *since = Some(at);
            set_developer(true);
            emit(
                "info",
                "developer",
                "developer mode on",
                json!({"reason":reason,"since":at.to_rfc3339(),"auto_off_hours":settings.auto_off_hours,
                    "profile":profile,"logs":isocast_diagnostics::logs_folder()}),
            );
        }
        (false, true) | (false, false) if developer() || since.is_some() => {
            *since = None;
            set_developer(false);
            *LAST_TRACE.lock() = None;
            let _ = std::fs::remove_file(since_file());
            emit(
                "info",
                "developer",
                "developer mode off",
                json!({"reason":reason,"profile":profile}),
            );
        }
        (false, false) if reason == "loaded" => {
            // Turned off while isocastd wasn't running.
            let _ = std::fs::remove_file(since_file());
        }
        _ => {}
    }
}

fn write_since(at: DateTime<Utc>) {
    let path = since_file();
    let temp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temp);
    if let Ok(mut file) = isocast_diagnostics::private_file(&temp) {
        use std::io::Write;
        if writeln!(file, "{}", json!({"since":at.to_rfc3339()})).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
    }
}

/// Turn developer mode on or off in the current profile (an ordinary
/// setup change, so every client sees it), then follow it at once.
pub(crate) fn set(daemon: &Daemon, params: DebugSetParams) -> Result<DeveloperStatus, RpcError> {
    if params.enabled.is_none() && params.auto_off_hours.is_none() {
        return Err(RpcError {
            code: RpcError::INVALID_PARAMS,
            message: "params: give `enabled` (true or false), `auto_off_hours` (0 for never), \
                      or both"
                .into(),
        });
    }
    if params.enabled == Some(true) {
        // Turning it on (again) starts its auto-off time afresh.
        *SINCE.lock() = None;
        let _ = std::fs::remove_file(since_file());
    }
    // Another client's change can land between reading the revision and
    // applying: read it again and retry, rather than fail the toggle.
    for attempt in 1.. {
        let (revision, current) = {
            let state = daemon.state.read();
            let setup = state
                .setup
                .as_ref()
                .ok_or_else(|| state.no_setup("developer mode is the profile's setting"))?;
            (state.revision, setup.developer)
        };
        let mut wanted = current;
        if let Some(enabled) = params.enabled {
            wanted.enabled = enabled;
        }
        if let Some(hours) = params.auto_off_hours {
            wanted.auto_off_hours = hours;
        }
        if wanted == current {
            break;
        }
        match daemon.apply(ApplyParams {
            revision,
            change: Change::Patch(json!({ "developer": wanted })),
            live: false,
        }) {
            Ok(_) => break,
            Err(error) if error.code == RpcError::WRONG_STATE && attempt < 3 => continue,
            Err(error) => return Err(error),
        }
    }
    sync(daemon, "debug_set");
    Ok(status(daemon))
}

pub(crate) fn status(daemon: &Daemon) -> DeveloperStatus {
    let (settings, profile) = settings(daemon);
    let since = *SINCE.lock();
    let folder = isocast_diagnostics::logs_folder();
    let log_bytes = isocast_diagnostics::files(&folder)
        .iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("-developer-"))
        })
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum();
    DeveloperStatus {
        enabled: settings.enabled,
        active: developer(),
        auto_off_hours: settings.auto_off_hours,
        since: since.map(|t| t.to_rfc3339()),
        auto_off_at: since
            .filter(|_| settings.auto_off_hours > 0)
            .map(|t| (t + chrono::Duration::hours(settings.auto_off_hours.into())).to_rfc3339()),
        profile,
        logs_folder: folder,
        log_bytes,
        log_budget_bytes: isocast_diagnostics::DEVELOPER_BUDGET,
        memory_bytes: memory_bytes(),
        trace: LAST_TRACE.lock().clone(),
    }
}

/// Once a second, from the diagnostics sampler: follow the setting, turn
/// it off when its time is up, and write the trace.
pub(crate) fn tick(daemon: &Daemon, status: &Status) {
    sync(daemon, "profile");
    let since = *SINCE.lock();
    let (settings, _) = settings(daemon);
    if let Some(since) = since
        && settings.auto_off_hours > 0
        && Utc::now() - since >= chrono::Duration::hours(settings.auto_off_hours.into())
    {
        let result = set(
            daemon,
            DebugSetParams {
                enabled: Some(false),
                auto_off_hours: None,
            },
        );
        match result {
            Ok(_) => emit(
                "info",
                "developer",
                "developer mode turned itself off",
                json!({"after_hours":settings.auto_off_hours}),
            ),
            // Retried next second (a change raced it, or a save failed).
            Err(error) => emit(
                "warn",
                "developer",
                "developer mode could not turn itself off",
                json!({"error":error.message}),
            ),
        }
        return;
    }
    if !developer() {
        return;
    }
    let (outputs, starts) = isocast_engine::trace::drain();
    let Some(engine) = &status.engine else {
        *LAST_TRACE.lock() = None;
        return;
    };
    let clocks: Vec<Value> = engine
        .clocks
        .iter()
        .map(|c| {
            json!({"id":c.id,"period_ms":c.period_ms,"mode":c.mode,"wake_lateness_us":c.wake_lateness_us,
                "wake_max_us":c.wake_max_us,"late_wakes":c.late_wakes})
        })
        .collect();
    #[cfg(target_os = "macos")]
    let encoders = isocast_platform_macos::diagnostics::encoder_metrics()["h264"].clone();
    #[cfg(not(target_os = "macos"))]
    let encoders = Value::Null;
    let record = json!({
        "interval_s": 1,
        "outputs": outputs,
        "encoder_start_order": starts,
        "program": {"fps":engine.fps,"frames":engine.program_frames,"missed":engine.missed_frames,
            "composite_wall_us":engine.compositor.draw_wall_us,"gpu_wait_us":engine.compositor.gpu_wait_us,
            "cpu_us":engine.compositor.cpu_us,"reused":engine.compositor.reused,"draws":engine.compositor.draws},
        "pacing": clocks,
        "stream_encoders": encoders,
        "recording": status.live.as_ref().map(|l| json!({"elapsed_s":l.elapsed_seconds,"paused":l.paused,
            "files":l.files.iter().map(|f| json!({"source":f.source,"frames":f.frames,"dropped":f.dropped_frames,
                "audio_dropouts":f.audio_dropouts})).collect::<Vec<_>>()})),
        "cpu_percent": status.cpu_percent,
        "memory_bytes": memory_bytes(),
    });
    emit_developer("info", "trace", "frame timing", record.clone());
    *LAST_TRACE.lock() = Some(record);
}

/// isocastd's memory as Activity Monitor shows it (its footprint); its
/// resident memory on Linux.
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
    #[cfg(target_os = "linux")]
    {
        isocast_diagnostics::resident_bytes()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}
