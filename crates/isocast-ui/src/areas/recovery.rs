//! Recovered recordings, in plain words: the notice after a crash (from
//! `status.interrupted`) saying what each file kept (or, with nothing to
//! recover, that the engine stopped unexpectedly), and the short toast
//! when a take's ISO or program file ends early while it records (from
//! `video_recovery`, #275).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use isocast_protocol::Setup;
use isocast_protocol::manifest::{
    InterruptedRecording, SessionManifest, VideoIncident, VideoRecoveryScope, VideoRecoveryState,
};

use crate::ToastKind;
use crate::app::App;
use crate::toasts::Toast;

/// What the recovery toasts have already said.
#[derive(Default)]
pub struct Recovery {
    /// Recovered takes told about this run (by manifest).
    told: RefCell<HashSet<String>>,
    /// The take (`record_epoch`) and its incidents told about.
    incidents: RefCell<(u64, HashSet<u64>)>,
    /// Engine launches that ended abnormally, from the daemon's warning,
    /// not yet told (by launch id).
    abnormal: RefCell<Vec<String>>,
}

/// The daemon's startup words for a recovered take, which this shows as
/// the recovered-take notice instead.
pub fn is_recovery_warning(message: &str) -> bool {
    message.starts_with("recording recovery inspected ")
        || message.starts_with("recording interrupted: ")
}

/// The launch id in the daemon's words for an engine launch that ended
/// abnormally (`previous daemon launch ended abnormally: "ID"; use
/// bug_report`, or isocastd's own `…: ID; use isocast report`).
pub fn abnormal_launch(message: &str) -> Option<&str> {
    let rest = message
        .strip_prefix("previous daemon launch ended abnormally: ")
        .or_else(|| message.strip_prefix("previous isocastd launch ended abnormally: "))?;
    let id = rest.split_once("; use ").map_or(rest, |(id, _)| id);
    Some(id.trim().trim_matches('"'))
}

impl Recovery {
    /// The daemon says an earlier engine launch ended abnormally: the
    /// recovered-take notice says so, or a plain one if nothing needed
    /// recovering (decided once the status is read).
    pub fn abnormal(&self, app: &Rc<App>, launch: &str) {
        let key = abnormal_key(launch);
        if self.told.borrow().contains(&key) || self.abnormal.borrow().iter().any(|l| l == launch) {
            return;
        }
        self.abnormal.borrow_mut().push(launch.to_string());
        app.refresh_status(true);
    }

    /// The status changed: tell about takes recovered at the engine's
    /// launch, and outputs that ended early in the take recording now.
    pub fn status_changed(&self, app: &Rc<App>) {
        let (interrupted, incidents, epoch, setup) = {
            let s = app.session.borrow();
            let Some(status) = s.status.as_ref() else {
                return;
            };
            // Status comes every second while recording: copy only what's new.
            let incidents: Vec<VideoIncident> = {
                let told = self.incidents.borrow();
                status
                    .live
                    .as_ref()
                    .and_then(|l| l.video_recovery.as_ref())
                    .into_iter()
                    .flat_map(|r| &r.incidents)
                    .filter(|i| told.0 != s.record_epoch || !told.1.contains(&i.incident_id))
                    .cloned()
                    .collect()
            };
            let interrupted: Vec<InterruptedRecording> = status
                .interrupted
                .iter()
                .filter(|t| t.newly_detected && !self.told.borrow().contains(&told_key(t)))
                .cloned()
                .collect();
            (interrupted, incidents, s.record_epoch, s.setup.clone())
        };
        let recovered_any = app
            .session
            .borrow()
            .status
            .as_ref()
            .is_some_and(|s| s.interrupted.iter().any(|t| t.newly_detected));
        self.tell_recovered(app, &interrupted, setup.as_deref());
        if setup.is_some() {
            self.tell_abnormal(app, recovered_any);
        }
        self.tell_ended_early(app, &incidents, epoch, setup.as_deref());
    }

    fn tell_recovered(&self, app: &Rc<App>, takes: &[InterruptedRecording], setup: Option<&Setup>) {
        // The files are named by their sources: wait for the setup.
        if takes.is_empty() || setup.is_none() {
            return;
        }
        // Newest first (as the daemon lists them); once per take, here and
        // in the prefs, so another window or launch doesn't say it again.
        let fresh: Vec<&InterruptedRecording> = takes
            .iter()
            .filter(|t| {
                let key = told_key(t);
                !self.told.borrow().contains(&key)
                    && (app.mode.offscreen || !crate::prefs::notice_hidden(&key))
            })
            .collect();
        if !app.mode.offscreen {
            for take in &fresh {
                crate::prefs::hide_notice(&told_key(take));
            }
        }
        // Takes told about at an earlier launch count too, so the prefs
        // aren't read again each second.
        for take in takes {
            self.told.borrow_mut().insert(told_key(take));
        }
        let Some(newest) = fresh.first() else {
            return;
        };
        let notice = recovered(&newest.recording, fresh.len() - 1, |id| {
            super::controls::file_label(setup, id)
        });
        let shown = newest.manifest.display().to_string();
        app.show_toast(
            Toast::new(notice.kind, notice.title, notice.message)
                .key("recovered-take")
                .rows(notice.rows)
                .action(crate::areas::remote::reveal_label(app), move |app| {
                    crate::areas::remote::reveal(app, &shown)
                })
                .action2("Report a Bug", |app| app.open_dialog("bug-report", "")),
        );
    }

    /// Launches that ended abnormally: the recovered-take notice covers
    /// them when this engine recovered a take; otherwise a plain notice.
    fn tell_abnormal(&self, app: &Rc<App>, recovered_any: bool) {
        let pending = std::mem::take(&mut *self.abnormal.borrow_mut());
        let fresh: Vec<String> = pending
            .into_iter()
            .map(|launch| abnormal_key(&launch))
            .filter(|key| app.mode.offscreen || !crate::prefs::notice_hidden(key))
            .collect();
        for key in &fresh {
            if !app.mode.offscreen {
                crate::prefs::hide_notice(key);
            }
            self.told.borrow_mut().insert(key.clone());
        }
        if fresh.is_empty() || recovered_any {
            return;
        }
        let (title, message) = stopped_unexpectedly();
        app.show_toast(
            Toast::new(ToastKind::Warning, title, message)
                // The recovered-take notice replaces it, should one follow.
                .key("recovered-take")
                .action("Report a Bug", |app| app.open_dialog("bug-report", "")),
        );
    }

    fn tell_ended_early(
        &self,
        app: &Rc<App>,
        incidents: &[VideoIncident],
        epoch: u64,
        setup: Option<&Setup>,
    ) {
        let mut told = self.incidents.borrow_mut();
        if told.0 != epoch {
            *told = (epoch, HashSet::new());
        }
        for incident in incidents {
            if told.1.contains(&incident.incident_id) {
                continue;
            }
            let Some((title, message)) =
                ended_early(incident, |id| super::controls::file_label(setup, id))
            else {
                continue;
            };
            told.1.insert(incident.incident_id);
            app.show_toast(
                Toast::new(ToastKind::Warning, title, message)
                    .key(format!("ended-early:{}", incident.incident_id)),
            );
        }
    }
}

fn abnormal_key(launch: &str) -> String {
    format!("abnormal:{launch}")
}

/// The notice for an engine that stopped unexpectedly with no recording
/// to recover.
pub fn stopped_unexpectedly() -> (&'static str, &'static str) {
    (
        "Isocast stopped unexpectedly last time",
        "There was no recording to recover.",
    )
}

fn told_key(take: &InterruptedRecording) -> String {
    format!("recovered:{}", take.manifest.display())
}

/// The recovered-take notice's words.
#[derive(Debug, PartialEq)]
pub struct Notice {
    pub kind: ToastKind,
    pub title: String,
    pub message: String,
    pub rows: Vec<(String, String, &'static str)>,
}

/// What a take recovered at launch kept: each file's length (or that it
/// couldn't be recovered), and outputs that had already ended early.
/// `earlier`: other takes recovered at the same launch.
pub fn recovered(take: &SessionManifest, earlier: usize, label: impl Fn(&str) -> String) -> Notice {
    let kept =
        |f: &&isocast_protocol::manifest::ManifestFile| f.duration_seconds.is_some_and(|d| d > 0.0);
    let total = take.files.len();
    let kept_files = take.files.iter().filter(kept).count();
    let longest = take
        .files
        .iter()
        .filter(kept)
        .filter_map(|f| f.duration_seconds)
        .fold(0.0, f64::max);
    // The program first, then the ISOs in the manifest's order.
    let mut files: Vec<_> = take.files.iter().collect();
    files.sort_by_key(|f| f.source != "program");
    let rows = files
        .into_iter()
        .map(|f| match f.duration_seconds.filter(|d| *d > 0.0) {
            Some(d) => (label(&f.source), format!("{} kept", length(d)), ""),
            None => (label(&f.source), "Couldn’t be recovered".to_string(), "bad"),
        })
        .collect();
    let mut sentences = Vec::new();
    let (kind, title) = if kept_files == 0 {
        sentences.push(if total == 1 {
            "Isocast stopped while recording, and its file couldn’t be recovered.".to_string()
        } else {
            "Isocast stopped while recording, and none of its files could be recovered.".into()
        });
        (
            ToastKind::Error,
            "Your last recording couldn’t be recovered",
        )
    } else {
        let files = if kept_files == total {
            plural(total, "file")
        } else {
            format!("{kept_files} of {total} files")
        };
        sentences.push(format!(
            "Isocast stopped while recording. {} kept, {} long; only what had reached the disk \
             was saved.",
            crate::app::sentence(&files),
            length(longest)
        ));
        (ToastKind::Warning, "Recovered your last recording")
    };
    let ended: Vec<String> = take
        .video_recovery
        .iter()
        .flat_map(|r| &r.incidents)
        .filter(|i| i.state == VideoRecoveryState::EndedEarly)
        .map(|i| format!("{} ended at {}", file_words(i, &label), at(i)))
        .collect();
    if !ended.is_empty() {
        sentences.push(format!("Earlier, {}.", list(&ended)));
    }
    if earlier > 0 {
        sentences.push(format!(
            "{} recovered too.",
            crate::app::sentence(&format!(
                "{} earlier {}",
                earlier,
                if earlier == 1 {
                    "take was"
                } else {
                    "takes were"
                }
            ))
        ));
    }
    Notice {
        kind,
        title: title.to_string(),
        message: sentences.join(" "),
        rows,
    }
}

/// The toast for an output that ended early while the take goes on:
/// ("Camera ISO stopped at 2:14", "The program keeps recording."). None
/// for an incident that didn't end a file.
pub fn ended_early(
    incident: &VideoIncident,
    label: impl Fn(&str) -> String,
) -> Option<(String, String)> {
    if incident.state != VideoRecoveryState::EndedEarly {
        return None;
    }
    let program = incident.scope == VideoRecoveryScope::Output && incident.owner_id == "program";
    let title = format!(
        "{} stopped at {}",
        crate::app::sentence(&file_words(incident, &label)),
        at(incident)
    );
    let message = if program {
        "The ISO files keep recording."
    } else {
        "The program keeps recording."
    };
    Some((title, message.to_string()))
}

/// "Camera ISO", or "the program file".
fn file_words(incident: &VideoIncident, label: impl Fn(&str) -> String) -> String {
    if incident.scope == VideoRecoveryScope::Output && incident.owner_id == "program" {
        "the program file".into()
    } else {
        format!("{} ISO", label(&incident.owner_id))
    }
}

/// Where on the take's saved timeline it was lost: "2:14", "1:02:05".
fn at(incident: &VideoIncident) -> String {
    let seconds = incident.first_lost_slot / u64::from(incident.fps.max(1));
    match seconds / 3600 {
        0 => format!("{}:{:02}", seconds / 60, seconds % 60),
        h => format!("{h}:{:02}:{:02}", seconds / 60 % 60, seconds % 60),
    }
}

/// A length people read: "45 s", "12 min 40 s", "1 h 2 min".
fn length(seconds: f64) -> String {
    let s = seconds.round() as u64;
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s} s"),
        (0, m, 0) => format!("{m} min"),
        (0, m, s) => format!("{m} min {s} s"),
        (h, 0, _) => format!("{h} h"),
        (h, m, _) => format!("{h} h {m} min"),
    }
}

fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// "a", "a and b", "a, b and c".
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn take(files: serde_json::Value, recovery: serde_json::Value) -> SessionManifest {
        serde_json::from_value(json!({
            "version": 1, "setup": "Weekly Stream", "status": "interrupted",
            "started_at": "2026-11-04T10:00:00-05:00", "host_time_ns": 0,
            "duration_seconds": null, "files": files, "errors": [],
            "video_recovery": recovery,
        }))
        .unwrap()
    }

    fn file(source: &str, duration: Option<f64>) -> serde_json::Value {
        json!({
            "path": format!("{source}.mp4"), "source": source, "kind": "camera",
            "width": 1920, "height": 1080, "fps": 30, "audio": [],
            "start_offset_seconds": 0.0, "duration_seconds": duration,
            "frames": null, "repeated_frames": null, "dropped_frames": null, "audio_dropouts": null,
        })
    }

    fn incident(scope: &str, owner: &str, state: &str, first: u64) -> serde_json::Value {
        json!({
            "incident_id": 1, "scope": scope, "owner_id": owner, "state": state,
            "fault_kind": "capture_stopped", "attempts_started": 0, "reason": "gone",
            "fps": 30, "first_lost_slot": first, "end_slot_exclusive": first + 30, "lost_slots": 30,
        })
    }

    fn label(id: &str) -> String {
        match id {
            "program" => "Program".into(),
            "camera" => "Camera".into(),
            other => other.into(),
        }
    }

    #[test]
    fn every_file_kept() {
        let notice = recovered(
            &take(
                json!([file("camera", Some(758.0)), file("program", Some(760.2))]),
                json!(null),
            ),
            0,
            label,
        );
        assert_eq!(notice.kind, ToastKind::Warning);
        assert_eq!(notice.title, "Recovered your last recording");
        assert_eq!(
            notice.message,
            "Isocast stopped while recording. 2 files kept, 12 min 40 s long; only what had \
             reached the disk was saved."
        );
        assert_eq!(
            notice.rows,
            vec![
                ("Program".into(), "12 min 40 s kept".into(), ""),
                ("Camera".into(), "12 min 38 s kept".into(), ""),
            ]
        );
    }

    #[test]
    fn a_lost_file_an_ended_iso_and_earlier_takes() {
        let notice = recovered(
            &take(
                json!([
                    file("program", Some(45.0)),
                    file("camera", Some(12.0)),
                    file("slides", None)
                ]),
                json!({"incident_count": 2, "history_limited": false, "incidents": [
                    incident("source", "camera", "ended_early", 360),
                    incident("source", "slides", "resumed", 30),
                ]}),
            ),
            1,
            label,
        );
        assert_eq!(notice.kind, ToastKind::Warning);
        assert_eq!(
            notice.message,
            "Isocast stopped while recording. 2 of 3 files kept, 45 s long; only what had \
             reached the disk was saved. Earlier, Camera ISO ended at 0:12. 1 earlier take was \
             recovered too."
        );
        assert_eq!(
            notice.rows[2],
            ("slides".into(), "Couldn’t be recovered".into(), "bad")
        );
    }

    #[test]
    fn nothing_kept() {
        let notice = recovered(&take(json!([file("program", None)]), json!(null)), 0, label);
        assert_eq!(notice.kind, ToastKind::Error);
        assert_eq!(notice.title, "Your last recording couldn’t be recovered");
        assert_eq!(
            notice.message,
            "Isocast stopped while recording, and its file couldn’t be recovered."
        );
    }

    #[test]
    fn ended_early_mid_take() {
        let iso: VideoIncident =
            serde_json::from_value(incident("source", "camera", "ended_early", 30 * 134)).unwrap();
        assert_eq!(
            ended_early(&iso, label),
            Some((
                "Camera ISO stopped at 2:14".into(),
                "The program keeps recording.".into()
            ))
        );
        let program: VideoIncident =
            serde_json::from_value(incident("output", "program", "ended_early", 30 * 3725))
                .unwrap();
        assert_eq!(
            ended_early(&program, label),
            Some((
                "The program file stopped at 1:02:05".into(),
                "The ISO files keep recording.".into()
            ))
        );
        let resumed: VideoIncident =
            serde_json::from_value(incident("source", "camera", "resumed", 30)).unwrap();
        assert_eq!(ended_early(&resumed, label), None);
    }

    #[test]
    fn abnormal_launches_are_recognised() {
        assert_eq!(
            abnormal_launch(
                "previous daemon launch ended abnormally: \"20261104T180821Z-11256-isocastd\"; use bug_report"
            ),
            Some("20261104T180821Z-11256-isocastd")
        );
        assert_eq!(
            abnormal_launch("previous isocastd launch ended abnormally: L-1; use isocast report"),
            Some("L-1")
        );
        assert_eq!(abnormal_launch("the disk is nearly full"), None);
        let (title, message) = stopped_unexpectedly();
        assert!(!format!("{title} {message}").contains("daemon"));
    }

    #[test]
    fn recovery_warnings_are_recognised() {
        assert!(is_recovery_warning(
            "recording recovery inspected C:\\x\\a.session.json; see the manifest"
        ));
        assert!(is_recovery_warning(
            "recording interrupted: /x/a.session.json; recoverable duration unknown"
        ));
        assert!(!is_recovery_warning("cam-1 dropped 3 frame(s) at 12 s"));
    }
}
