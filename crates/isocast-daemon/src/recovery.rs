//! Failure boundaries and orderly shutdown after poisoned shared state.
use crate::Daemon;
use isocast_protocol::api::{Event, socket_path};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

impl Daemon {
    pub(crate) fn contain(self: &Arc<Self>, name: &str, run: impl FnOnce()) {
        if let Err(message) = isocast_diagnostics::contain(name, run) {
            self.worker_failed(message);
        }
    }

    pub(crate) fn worker_failed(self: &Arc<Self>, message: String) {
        isocast_diagnostics::emit(
            "error",
            "recovery",
            "worker panic contained",
            serde_json::json!({"error":message}),
        );
        if self.state.is_poisoned() {
            self.emergency_stop(message);
        } else {
            self.publish(Event::Warning { message });
        }
    }

    /// Never unwind through the recorder's ownership. Extract only stop
    /// handles from poisoned state, finalize outside it, and then terminate.
    pub(crate) fn emergency_stop(self: &Arc<Self>, why: String) {
        if self.emergency.swap(true, Ordering::SeqCst) {
            return;
        }
        let daemon = self.clone();
        std::thread::spawn(move || {
            let finished = Arc::new(AtomicBool::new(false));
            let watchdog = finished.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(10));
                if !watchdog.load(Ordering::SeqCst) {
                    isocast_diagnostics::emit(
                        "error",
                        "recovery",
                        "emergency finalization timed out; durable fragments retained",
                        serde_json::Value::Null,
                    );
                    // Do not run possibly poisoned destructors or manufacture
                    // SIGABRT. No clean marker: startup will repair manifests.
                    std::process::exit(70);
                }
            });
            isocast_diagnostics::emit(
                "error",
                "recovery",
                "emergency stop: finalizing recording and stream",
                serde_json::json!({"reason":why}),
            );
            let (id, stream, engine) = {
                let mut state = daemon.state.lock().unwrap_or_else(|p| p.into_inner());
                let id = state.session.as_ref().map(|s| {
                    s.stop.store(true, Ordering::SeqCst);
                    s.id
                });
                (id, state.stream.take(), state.engine.clone())
            };
            // Stop the stream as well, while its sources still run.
            if let Some(stream) = stream {
                let _ = isocast_diagnostics::contain("emergency stream stop", || stream.stop());
            }
            let deadline = Instant::now() + Duration::from_secs(9);
            let report = loop {
                let state = daemon.state.lock().unwrap_or_else(|p| p.into_inner());
                if id.is_none_or(|id| state.ended_session >= id) {
                    break state.last.clone();
                }
                drop(state);
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            if let Some(report) = &report {
                isocast_engine::record::mark_interrupted(report, &why);
            }
            if let Some(engine) = engine {
                let _ = isocast_diagnostics::contain("emergency engine stop", || engine.stop());
            }
            // Persist an explicit emergency reason independent of the log disk.
            let path = isocast_protocol::api::setup_path().with_extension("emergency.json");
            let safe = isocast_diagnostics::redact::Redactor::default().value(
                &serde_json::json!({"reason":why,"recording":report,"at":crate::now().as_nanos()}),
            );
            let _ = std::fs::write(path, safe.to_string());
            isocast_diagnostics::emit(
                "error",
                "recovery",
                "emergency finalization finished",
                serde_json::json!({"reason":why,"recording_finalized":report.is_some()}),
            );
            let _ = std::fs::remove_file(socket_path());
            finished.store(true, Ordering::SeqCst);
            // This was orderly media finalization, but an abnormal daemon exit.
            std::process::exit(70);
        });
    }

    /// The recordings folders recordings were made in lately (newest
    /// last), as `remember_recording_folder` keeps them.
    pub(crate) fn remembered_recording_folders(&self) -> Vec<PathBuf> {
        let path = isocast_protocol::api::setup_path().with_extension("recording-folders.json");
        std::fs::read(path)
            .ok()
            .and_then(|s| serde_json::from_slice::<Vec<PathBuf>>(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn remember_recording_folder(&self, folder: &std::path::Path) {
        let path = isocast_protocol::api::setup_path().with_extension("recording-folders.json");
        let mut folders = std::fs::read(&path)
            .ok()
            .and_then(|s| serde_json::from_slice::<Vec<PathBuf>>(&s).ok())
            .unwrap_or_default();
        folders.retain(|p| p != folder);
        folders.push(folder.to_owned());
        if folders.len() > 20 {
            folders.drain(..folders.len() - 20);
        }
        let _ = std::fs::write(path, serde_json::to_vec(&folders).unwrap_or_default());
    }

    pub(crate) fn recover_recordings(&self) {
        // Each recordings folder, and every project folder in it.
        let mut folders: Vec<PathBuf> = self
            .recording_roots()
            .iter()
            .flat_map(|root| isocast_protocol::project::recording_folders(root, 50))
            .collect();
        folders.sort();
        folders.dedup();
        #[cfg(target_os = "macos")]
        let mut recovered = folders
            .iter()
            .flat_map(|p| {
                isocast_engine::record::recover_interrupted(&isocast_platform_macos::MacPlatform, p)
            })
            .collect::<Vec<_>>();
        #[cfg(not(target_os = "macos"))]
        let recovered = Vec::new();
        recovered.sort_by(|a, b| b.recording.started_at.cmp(&a.recording.started_at));
        recovered.truncate(20);
        for take in recovered.iter().filter(|r| r.newly_detected) {
            // Files a crash-safe writer left were finished from their
            // fragments just now; the manifest's errors say what each holds.
            let finished = take.recording.errors.iter().any(|e| {
                e.contains("crash-safe fragments that reached the disk")
                    || e.contains("completed at launch")
            });
            self.publish(Event::Warning {
                message: if finished {
                    format!(
                        "recording interrupted: {}; its files were finished from what reached the disk (see the manifest's errors and status.interrupted)",
                        take.manifest.display(),
                    )
                } else {
                    format!(
                        "recording interrupted: {}; recoverable duration unknown; inspect status.interrupted or bug_report",
                        take.manifest.display(),
                    )
                },
            });
        }
        *self.interrupted_unnoted.lock().unwrap() = recovered
            .iter()
            .filter(|r| r.newly_detected)
            .map(|take| take.manifest.clone())
            .collect();
        *self.interrupted.lock().unwrap() = recovered;
    }
}
