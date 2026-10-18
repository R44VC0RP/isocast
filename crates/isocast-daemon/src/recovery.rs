//! Failure boundaries and orderly shutdown after broken shared state.
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
        // Only a panic that cut a change to the control state short leaves
        // it possibly inconsistent; one that only read it (housekeeping)
        // leaves recordings and the stream running.
        if self.state.is_broken() {
            self.emergency_stop(message);
        } else {
            self.publish(Event::Warning { message });
        }
    }

    /// Never unwind through the recorder's ownership. Extract only stop
    /// handles from broken state, finalize outside it, and then terminate.
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
                    // Do not run destructors of broken state or manufacture
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
                let mut state = daemon.state.lock();
                let id = state.session.as_ref().map(|s| {
                    s.stop.store(true, Ordering::SeqCst);
                    s.id
                });
                (id, std::mem::take(&mut state.streams), state.engine.clone())
            };
            // Stop the streams as well, while their sources still run.
            for (_, output) in stream {
                if let crate::streams::Output::Running(stream) = output {
                    let _ = isocast_diagnostics::contain("emergency stream stop", || stream.stop());
                }
            }
            let deadline = Instant::now() + Duration::from_secs(9);
            let report = loop {
                let state = daemon.state.read();
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
        isocast_protocol::project::remembered_roots()
    }

    /// Remember a recordings folder (a failure is logged: the folder's
    /// projects are then listed only while it is the setup's).
    pub(crate) fn remember_recording_folder(&self, folder: &std::path::Path) {
        if let Err(err) = isocast_protocol::project::remember_root(folder) {
            isocast_diagnostics::diagnostic!("warn";
                "isocastd: could not remember the recordings folder {}: {err}",
                folder.display()
            );
        }
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
        let mut recovered = folders
            .iter()
            .flat_map(|p| {
                isocast_engine::record::recover_interrupted(&crate::platform::NativePlatform, p)
            })
            .collect::<Vec<_>>();
        recovered.sort_by(|a, b| b.recording.started_at.cmp(&a.recording.started_at));
        recovered.truncate(20);
        for take in recovered.iter().filter(|r| r.newly_detected) {
            #[cfg(windows)]
            self.publish(Event::Warning {
                message: format!(
                    "recording recovery inspected {}; see the manifest and status.interrupted for each file's verified media, unknown counts and any persistence errors",
                    take.manifest.display(),
                ),
            });
            #[cfg(not(windows))]
            {
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
        }
        *self.interrupted_unnoted.lock() = recovered
            .iter()
            .filter(|r| r.newly_detected)
            .map(|take| take.manifest.clone())
            .collect();
        *self.interrupted.lock() = recovered;
    }
}
