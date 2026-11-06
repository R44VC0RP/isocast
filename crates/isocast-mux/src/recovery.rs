//! Manifest recovery through one pinned, exclusively leased directory.
//! Packet indexes, not accepted input counts or requested durations, are facts.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use isocast_protocol::manifest::{
    InterruptedRecording, MANIFEST_VERSION, ManifestFile, RolloverState, SessionManifest,
    SessionStatus, VideoFaultKind, VideoIncident, VideoRecoveryScope, VideoRecoveryState,
};
use serde_json::Value;

use crate::{file::Directory, mp4};

const MAX_MANIFEST: usize = 4 << 20;
const MAX_SESSIONS: usize = 100;
#[cfg(windows)]
const NOTE: &str = "Windows recovery: ";
#[cfg(target_os = "linux")]
const NOTE: &str = "Linux recovery: ";

#[derive(Default)]
pub struct Recovery {
    pub sessions: Vec<InterruptedRecording>,
    pub warnings: Vec<String>,
    /// At least one manifest/media file could not be verified and persisted,
    /// or the bounded scan may have omitted a take. Retain recovery metadata.
    pub incomplete: bool,
}

/// Atomic manifest replacement for live recording checkpoints. The caller's
/// existing shared recording lease can coexist with this one.
pub fn write_manifest(path: &Path, manifest: &SessionManifest, durable: bool) -> Result<()> {
    let name = path
        .file_name()
        .context("manifest path needs a file name")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = Directory::for_recording(parent)?;
    write_manifest_in(&directory, name, manifest, durable)
        .with_context(|| format!("could not write {}", path.display()))
}

/// Live recording checkpoints retain the caller's original directory lease.
pub fn write_manifest_in(
    directory: &Directory,
    name: &OsStr,
    manifest: &SessionManifest,
    durable: bool,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MANIFEST {
        bail!("manifest exceeds 4 MiB");
    }
    directory.replace_metadata(name, &bytes, durable)
}

/// Durable final outcome, written before attempting the publicly polled name.
/// Keep this sidecar after publication: it also survives a process exit between
/// the two writes. Recovery only adopts it for the same unfinished take.
pub fn save_final_in(
    directory: &Directory,
    name: &OsStr,
    manifest: &SessionManifest,
) -> Result<()> {
    if manifest.status == SessionStatus::Recording {
        bail!("final manifest must have complete, failed or interrupted status");
    }
    write_manifest_in(directory, &final_name(name), manifest, true)
}

fn final_name(name: &OsStr) -> OsString {
    let mut name = name.to_os_string();
    name.push(".final");
    name
}

fn read_final(
    directory: &Directory,
    name: &OsStr,
    original: &SessionManifest,
) -> Result<Option<(Value, SessionManifest)>> {
    if original.status != SessionStatus::Recording {
        return Ok(None);
    }
    let saved = match read_manifest(directory, &final_name(name)) {
        Ok(saved) => saved,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).context("could not read final manifest sidecar"),
    };
    let manifest = &saved.1;
    // Failed handoffs can remove never-opened files from the original plan;
    // every retained file must still belong to this same take.
    if manifest.status == SessionStatus::Recording
        || manifest.setup != original.setup
        || manifest.started_at != original.started_at
        || manifest.host_time_ns != original.host_time_ns
        || !manifest.files.iter().all(|a| {
            original
                .files
                .iter()
                .any(|b| a.path == b.path && a.source == b.source)
        })
    {
        bail!("final manifest sidecar does not describe the same finished take; left untouched");
    }
    Ok(Some(saved))
}

fn publish_document(directory: &Directory, name: &OsStr, document: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(document)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MANIFEST {
        bail!("manifest exceeds 4 MiB");
    }
    directory.replace_metadata(name, &bytes, true)
}

/// An emergency stop may have finished media normally. Preserve those known
/// counts and unknown extension fields while changing only status and reason.
pub fn mark_interrupted(path: &Path, why: &str) -> Result<()> {
    let name = path
        .file_name()
        .context("manifest path needs a file name")?;
    let folder = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = Directory::for_recording(folder)?;
    let (mut value, mut manifest) = read_manifest(&directory, name)?;
    if let Some((saved, _)) = read_final(&directory, name, &manifest)? {
        return publish_document(&directory, name, &saved);
    }
    if !manifest.errors.iter().any(|e| e == why) {
        manifest.errors.push(why.to_string());
    }
    value["status"] = serde_json::to_value(SessionStatus::Interrupted)?;
    value["errors"] = serde_json::to_value(manifest.errors)?;
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MANIFEST {
        bail!("manifest exceeds 4 MiB");
    }
    directory.replace_metadata(name, &bytes, true)
}

/// At Start, only newly orphaned `recording` manifests need inspection. Leave
/// historical failed/interrupted media to launch recovery, not two full scans
/// on every new record-start path. A read-only listing just identifies orphans.
pub fn interrupted_sessions(folder: &Path, mark: bool) -> Result<Vec<PathBuf>> {
    if mark {
        return Ok(recover_with(folder, true)?
            .sessions
            .into_iter()
            .filter(|s| s.newly_detected)
            .map(|s| s.manifest)
            .collect());
    }
    let directory = Directory::for_repair(folder)?;
    Ok(names(&directory)?
        .into_iter()
        .filter_map(|name| {
            let (_, manifest) = read_manifest(&directory, &name).ok()?;
            if read_final(&directory, &name, &manifest)
                .ok()
                .flatten()
                .is_some()
            {
                return None;
            }
            (manifest.status == SessionStatus::Recording).then(|| folder.join(name))
        })
        .collect())
}

/// Keep this lease through enumeration, all reads/repairs and atomic replacement.
/// Returned paths retain the caller's spelling for UI/root matching; they are
/// labels, never reopened by this pass. Failed replacements are reported.
pub fn recover_directory(folder: &Path) -> Result<Recovery> {
    recover_with(folder, false)
}

fn recover_with(folder: &Path, recording_only: bool) -> Result<Recovery> {
    let directory = Directory::for_repair(folder)?;
    let mut result = Recovery::default();
    let names = names(&directory)?;
    result.incomplete = names.len() == MAX_SESSIONS;
    for name in names {
        let (mut document, mut manifest) = match read_manifest(&directory, &name) {
            Ok(value) => value,
            Err(error) => {
                result.incomplete = true;
                result
                    .warnings
                    .push(format!("{}: {error:#}", name.to_string_lossy()));
                continue;
            }
        };
        match read_final(&directory, &name, &manifest) {
            Ok(Some((saved, finished))) => {
                if let Err(error) = publish_document(&directory, &name, &saved) {
                    result.warnings.push(format!(
                        "{}: final outcome is safe in {} but publication is still pending: {error:#}",
                        name.to_string_lossy(), final_name(&name).to_string_lossy()
                    ));
                }
                // Never downgrade a durably finished take to interrupted, even
                // if the external reader is still present on this startup.
                if finished.status == SessionStatus::Complete || recording_only {
                    continue;
                }
                // Failed writers may still have repairable fragments. Preserve
                // the existing full-startup recovery of failed recordings.
                document = saved;
                manifest = finished;
            }
            Ok(None) => {}
            Err(error) => result
                .warnings
                .push(format!("{}: {error:#}", name.to_string_lossy())),
        }
        if recording_only && manifest.status != SessionStatus::Recording {
            continue;
        }
        if !matches!(
            manifest.status,
            SessionStatus::Recording | SessionStatus::Interrupted | SessionStatus::Failed
        ) {
            continue;
        }
        let original = manifest.clone();
        let newly_detected = manifest.status == SessionStatus::Recording;
        if newly_detected {
            manifest.status = SessionStatus::Interrupted;
            note(&mut manifest.errors, "the recording process ended without finishing; only verified packet indexes can establish saved media".into());
            if let Some(pause) = manifest.pauses.last().filter(|p| p.end_seconds.is_none()) {
                let message = match pause.recoverable_seconds_at_pause {
                    Some(at) => format!(
                        "the take ended while paused; its earlier flushed commitment was {at:.9} s"
                    ),
                    None => "the take ended while paused without a verified commitment".into(),
                };
                note(&mut manifest.errors, message);
            }
        }
        let mut repaired = false;
        let mut all_verified = true;
        let mut verified_ends = Vec::with_capacity(manifest.files.len());
        let mut empty_rows = Vec::new();
        for (index, file) in manifest.files.iter_mut().enumerate() {
            // An unfinished live checkpoint holds accepted inputs, not saved
            // media. A previously finalized/recovered take can hold real facts;
            // temporary inaccessibility must not erase those on the next launch.
            if newly_detected {
                file.frames = None;
                file.repeated_frames = None;
                file.dropped_frames = None;
                file.audio_dropouts = None;
                file.duration_seconds = None;
            }
            let outcome = (|| -> Result<(mp4::Recovered, bool)> {
                let media = directory.open_for_read(OsStr::new(&file.path))?;
                let inspected = mp4::inspect(&media)?
                    .context("file is not an isw2 encoded recording; media left untouched")?;
                if inspected.complete {
                    return Ok((inspected.media, false));
                }
                drop(media);
                // Revalidate from the same pinned directory under writable
                // ownership; an external rename cannot make inspection authorize
                // mutation of a different/foreign file.
                let mut media = directory.open_for_repair(OsStr::new(&file.path))?;
                if let Some(saved) = mp4::recover(&mut media)? {
                    return Ok((saved, true));
                }
                let inspected = mp4::inspect(&media)?
                    .context("file is not an isw2 encoded recording; left untouched")?;
                Ok((inspected.media, false))
            })();
            let intent = manifest
                .video_recovery
                .as_mut()
                .and_then(|ledger| ledger.incidents.iter_mut().find(|e| pending_part(e, file)));
            match outcome {
                Ok((saved, changed)) => {
                    repaired |= changed;
                    file.frames = saved.pictures();
                    file.duration_seconds = saved.longest();
                    if let Some(part) = file.part.as_mut() {
                        part.video_duration_seconds = saved
                            .tracks
                            .iter()
                            .find(|t| t.video && t.samples > 0)
                            .map(|t| t.seconds);
                        // Intent precedes media; only verified indexed video
                        // can promote it. Never infer a resume from a name,
                        // an empty reservation or an audio-only fragment.
                        if part.video_duration_seconds.is_some()
                            && let Some(e) = intent
                            && e.state == VideoRecoveryState::Recovering
                            && e.fault_kind == VideoFaultKind::EncoderFailed
                        {
                            let r = e.rollover.as_mut().expect("matched intent");
                            // Same exclusive grid edge as engine file_end:
                            // floor(n * 1e9 / fps) + 1ns >= offset.
                            let slot = (u128::from(r.take_offset_ns.saturating_sub(1))
                                * u128::from(e.fps))
                            .div_ceil(1_000_000_000) as u64;
                            if slot >= e.first_lost_slot {
                                r.state = RolloverState::Committed;
                                e.state = VideoRecoveryState::Resumed;
                                e.end_slot_exclusive = slot;
                                e.lost_slots = slot - e.first_lost_slot;
                            }
                        }
                    }
                    if changed && saved.rebuilt {
                        file.repeated_frames = None;
                        file.dropped_frames = None;
                        file.audio_dropouts = None;
                    }
                    // A pause promise covers every configured track, including
                    // ones with no packets. The longest movie end cannot prove it.
                    let shape_matches = saved.tracks.iter().filter(|t| t.video).count() == 1
                        && saved.tracks.iter().filter(|t| !t.video).count() == file.audio.len();
                    verified_ends.push(
                        (shape_matches && saved.tracks.iter().all(|t| t.samples > 0))
                            .then(|| {
                                saved
                                    .tracks
                                    .iter()
                                    .map(|t| t.seconds)
                                    .reduce(f64::min)
                                    .zip(
                                        file.start_offset_seconds
                                            .filter(|v| v.is_finite() && *v >= 0.0),
                                    )
                                    .map(|(duration, offset)| duration + offset)
                            })
                            .flatten(),
                    );
                    if !shape_matches {
                        note(
                            &mut manifest.errors,
                            format!(
                                "{}: indexed video/audio track count differs from the manifest; source labels cannot be verified",
                                file.path
                            ),
                        );
                    }
                    note(
                        &mut manifest.errors,
                        format!(
                            "{}: indexed media verified at launch from {} crash-safe fragments that reached the disk: {}; indexes do not establish drop/repeat/audio-loss counts",
                            file.path,
                            saved.fragments,
                            saved.summary()
                        ),
                    );
                }
                Err(error) => {
                    if error.is::<mp4::EmptyReservation>()
                        && let Some(e) = intent
                        && file.frames.is_none()
                        && file.duration_seconds.is_none()
                    {
                        e.rollover.as_mut().expect("matched intent").state =
                            RolloverState::ReservedNoMedia;
                        if e.state == VideoRecoveryState::Recovering {
                            e.state = VideoRecoveryState::Stopped;
                        }
                        empty_rows.push(index);
                        continue;
                    }
                    all_verified = false;
                    verified_ends.push(None);
                    note(
                        &mut manifest.errors,
                        format!(
                            "{}: media could not be verified or finished; stored counts/duration, if any, are historical and not revalidated: {error:#}",
                            file.path,
                        ),
                    );
                }
            }
        }
        // Keep the header untouched; we have no recording-time ownership of
        // that directory entry. Remove its row from both representations to
        // preserve unknown fields on the *same* surviving physical files.
        for index in empty_rows.into_iter().rev() {
            manifest.files.remove(index);
            document["files"]
                .as_array_mut()
                .context("expected manifest files")?
                .remove(index);
        }
        // Unknown files or alignment mean an unknown session end, not zero or
        // the maximum of whichever files happened to be readable.
        if newly_detected || all_verified {
            manifest.duration_seconds = manifest
                .files
                .iter()
                .try_fold(0.0f64, |end, file| {
                    let duration = file
                        .duration_seconds
                        .filter(|v| v.is_finite() && *v >= 0.0)?;
                    let offset = file
                        .start_offset_seconds
                        .filter(|v| v.is_finite() && *v >= 0.0)?;
                    let value = duration + offset;
                    value.is_finite().then(|| end.max(value))
                })
                .filter(|_| !manifest.files.is_empty());
        }
        for pause in &manifest.pauses {
            if let Some(promised) = pause.recoverable_seconds_at_pause {
                let covered = promised.is_finite()
                    && promised >= 0.0
                    && !verified_ends.is_empty()
                    && verified_ends
                        .iter()
                        .all(|end| end.is_some_and(|end| end + 1e-6 >= promised));
                if !covered {
                    note(
                        &mut manifest.errors,
                        format!(
                            "the earlier pause commitment of {promised:.9} s cannot be verified for every file; no shorter promise was substituted"
                        ),
                    );
                }
            }
        }
        let changed = manifest != original;
        let mut persisted = true;
        if changed {
            update_document(&mut document, &manifest)?;
            let mut bytes = serde_json::to_vec_pretty(&document)?;
            bytes.push(b'\n');
            let written = if bytes.len() > MAX_MANIFEST {
                Err(anyhow::anyhow!("recovered manifest exceeds 4 MiB"))
            } else {
                directory.replace_metadata(&name, &bytes, true)
            };
            if let Err(error) = written {
                persisted = false;
                note(
                    &mut manifest.errors,
                    format!("could not persist interrupted marker: {error:#}"),
                );
                result.warnings.push(format!(
                    "{}: could not persist recovery: {error:#}",
                    name.to_string_lossy()
                ));
            }
        }
        if manifest.status == SessionStatus::Interrupted
            || manifest.errors.iter().any(|e| e.starts_with(NOTE))
        {
            result.sessions.push(InterruptedRecording {
                newly_detected: newly_detected || repaired || changed || !persisted,
                manifest: folder.join(&name),
                recording: manifest,
            });
        }
        result.incomplete |= !all_verified || !persisted;
    }
    Ok(result)
}

fn pending_part(e: &VideoIncident, file: &ManifestFile) -> bool {
    e.scope == VideoRecoveryScope::Output
        && e.owner_id == file.source
        && e.fps == file.fps
        && (1..=240).contains(&e.fps)
        && e.rollover.as_ref().is_some_and(|r| {
            r.state == RolloverState::Pending
                && r.path == file.path
                && r.part > 1
                && file.part.as_ref().is_some_and(|p| p.number == r.part)
                && file
                    .start_offset_seconds
                    .is_some_and(|s| (s - r.take_offset_ns as f64 / 1e9).abs() < 1e-8)
        })
}

fn names(directory: &Directory) -> Result<Vec<OsString>> {
    directory.manifest_names(MAX_SESSIONS)
}

fn read_manifest(directory: &Directory, name: &OsStr) -> Result<(Value, SessionManifest)> {
    let file = directory.open_for_read(name)?;
    if file.metadata()?.len() > MAX_MANIFEST as u64 {
        bail!("manifest exceeds 4 MiB");
    }
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MANIFEST {
        bail!("manifest grew beyond 4 MiB");
    }
    let value: Value =
        serde_json::from_slice(&bytes).context("unreadable manifest JSON; left untouched")?;
    let manifest: SessionManifest =
        serde_json::from_value(value.clone()).context("invalid manifest; left untouched")?;
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "unsupported manifest version {}; left untouched",
            manifest.version
        );
    }
    Ok((value, manifest))
}

fn note(errors: &mut Vec<String>, message: String) {
    let message = format!("{NOTE}{message}");
    if !errors.contains(&message) {
        errors.push(message);
    }
}

/// Retain unknown fields (including nested per-file extensions). Recovery only
/// owns these outcome fields; it does not reinterpret source/settings metadata.
fn update_document(value: &mut Value, manifest: &SessionManifest) -> Result<()> {
    let object = value.as_object_mut().context("manifest is not an object")?;
    object.insert("status".into(), serde_json::to_value(manifest.status)?);
    object.insert(
        "duration_seconds".into(),
        serde_json::to_value(manifest.duration_seconds)?,
    );
    object.insert("errors".into(), serde_json::to_value(&manifest.errors)?);
    if let Some(known) = &manifest.video_recovery
        && let Some(rows) = object
            .get_mut("video_recovery")
            .and_then(|v| v.get_mut("incidents"))
            .and_then(Value::as_array_mut)
    {
        for (row, e) in rows.iter_mut().zip(&known.incidents) {
            if let Some(r) = &e.rollover
                && matches!(
                    r.state,
                    RolloverState::Committed | RolloverState::ReservedNoMedia
                )
                && row["rollover"]["state"] == "pending"
            {
                row["rollover"]["state"] = serde_json::to_value(r.state)?;
                row["state"] = serde_json::to_value(e.state)?;
                if r.state == RolloverState::ReservedNoMedia {
                    continue;
                }
                row["end_slot_exclusive"] = e.end_slot_exclusive.into();
                row["lost_slots"] = e.lost_slots.into();
            }
        }
    }
    let files = object
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .context("manifest files is not an array")?;
    for (value, file) in files.iter_mut().zip(&manifest.files) {
        let object = value
            .as_object_mut()
            .context("manifest file is not an object")?;
        if let Some(part) = &file.part
            && let Some(saved) = object.get_mut("part").and_then(Value::as_object_mut)
        {
            saved.insert(
                "video_duration_seconds".into(),
                serde_json::to_value(part.video_duration_seconds)?,
            );
        }
        object.insert("frames".into(), serde_json::to_value(file.frames)?);
        object.insert(
            "repeated_frames".into(),
            serde_json::to_value(file.repeated_frames)?,
        );
        object.insert(
            "dropped_frames".into(),
            serde_json::to_value(file.dropped_frames)?,
        );
        object.insert(
            "audio_dropouts".into(),
            serde_json::to_value(file.audio_dropouts)?,
        );
        object.insert(
            "duration_seconds".into(),
            serde_json::to_value(file.duration_seconds)?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn empty_rollover_is_never_promoted_and_a_torn_movie_is_not_dismissed() {
        let folder = std::env::temp_dir().join(format!(
            "isocast-empty-rollover-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let name = "program.part002.mov";
        let movie = folder.join(name);
        drop(mp4::Writer::create(&movie, isocast_protocol::Container::Mov, UNIX_EPOCH).unwrap());
        let header = std::fs::read(&movie).unwrap();
        let manifest = folder.join("take.session.json");
        let intent = serde_json::json!({
            "version":1, "setup":"roll", "status":"recording",
            "started_at":"2026-11-06T00:00:00Z", "host_time_ns":1, "errors":[],
            "files":[{"source":"program", "kind":"program", "path":name, "part":{"number":2},
                "fps":30, "width":160, "height":90, "audio":[], "start_offset_seconds":2.0}],
            "video_recovery":{"history_limited":false,"incident_count":1,"incidents":[{
                "incident_id":1,"scope":"output","owner_id":"program","state":"recovering",
                "fault_kind":"encoder_failed","attempts_started":1,"reason":"encoder failure",
                "fps":30,"first_lost_slot":55,"end_slot_exclusive":60,"lost_slots":5,
                "rollover":{"state":"pending","part":2,"path":name,"take_offset_ns":2_000_000_000u64}
            }]}
        });
        for torn in [true, false] {
            let mut bytes = header.clone();
            if torn {
                bytes.push(0);
            }
            std::fs::write(&movie, &bytes).unwrap();
            std::fs::write(&manifest, serde_json::to_vec(&intent).unwrap()).unwrap();
            let recovered = recover_directory(&folder).unwrap();
            assert_eq!(recovered.incomplete, torn);
            let first = std::fs::read(&manifest).unwrap();
            let saved: Value = serde_json::from_slice(&first).unwrap();
            assert_eq!(saved["files"].as_array().unwrap().len(), usize::from(torn));
            let e = &saved["video_recovery"]["incidents"][0];
            assert_ne!(e["state"], "resumed");
            assert_eq!(
                e["rollover"]["state"],
                if torn { "pending" } else { "reserved_no_media" }
            );
            let again = recover_directory(&folder).unwrap();
            assert_eq!(again.incomplete, torn);
            assert!(!again.sessions[0].newly_detected);
            assert_eq!(std::fs::read(&manifest).unwrap(), first);
            assert_eq!(std::fs::read(&movie).unwrap(), bytes);
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn final_sidecar_preserves_extensions_and_cannot_replace_another_take() {
        let folder = std::env::temp_dir().join(format!(
            "isocast-final-recovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let name = OsStr::new("take.session.json");
        let path = folder.join(name);
        let initial: SessionManifest = serde_json::from_value(serde_json::json!({
            "version":1,"setup":"sidecar","status":"recording",
            "started_at":"2026-10-25T12:00:00Z","host_time_ns":1,"files":[],"errors":[]
        }))
        .unwrap();
        let mut final_manifest = initial.clone();
        final_manifest.status = SessionStatus::Complete;
        final_manifest.duration_seconds = Some(18.0);
        let mut final_value = serde_json::to_value(&final_manifest).unwrap();
        final_value["extension"] = serde_json::json!({"kept": true});
        {
            let directory = Directory::for_recording(&folder).unwrap();
            write_manifest_in(&directory, name, &initial, true).unwrap();
            publish_document(&directory, &final_name(name), &final_value).unwrap();
            assert!(save_final_in(&directory, name, &initial).is_err());
        }
        assert!(interrupted_sessions(&folder, false).unwrap().is_empty());
        let recovered = recover_directory(&folder).unwrap();
        assert!(recovered.sessions.is_empty() && recovered.warnings.is_empty());
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, final_value);
        {
            let directory = Directory::for_recording(&folder).unwrap();
            let mut other = initial;
            other.host_time_ns += 1;
            write_manifest_in(&directory, name, &other, true).unwrap();
        }
        let recovered = recover_directory(&folder).unwrap();
        assert!(
            recovered
                .warnings
                .iter()
                .any(|w| w.contains("does not describe the same finished take"))
        );
        let saved: SessionManifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.status, SessionStatus::Interrupted);
        assert_ne!(saved.host_time_ns, final_manifest.host_time_ns);
        // Complete and emergency-stopped outcomes can omit a planned file
        // that never opened. Preserve their exact facts and interruption reason.
        for status in [SessionStatus::Complete, SessionStatus::Interrupted] {
            let mut finished = final_manifest.clone();
            finished.status = status;
            finished.files.push(serde_json::from_value(serde_json::json!({
                "path":"program.mov","source":"program","kind":"program",
                "width":320,"height":180,"fps":30,"audio":[],"frames":540,
                "duration_seconds":18.0,"dropped_frames":0,"repeated_frames":0,"audio_dropouts":0
            })).unwrap());
            if status == SessionStatus::Interrupted {
                finished
                    .errors
                    .push("recording interrupted: emergency stop".into());
            }
            {
                let directory = Directory::for_recording(&folder).unwrap();
                let mut initial = finished.clone();
                initial.status = SessionStatus::Recording;
                initial.files.push(
                    serde_json::from_value(serde_json::json!({
                        "path":"unopened.mov","source":"program","kind":"program",
                        "width":320,"height":180,"fps":30,"audio":[]
                    }))
                    .unwrap(),
                );
                write_manifest_in(&directory, name, &initial, true).unwrap();
                save_final_in(&directory, name, &finished).unwrap();
            }
            assert!(interrupted_sessions(&folder, true).unwrap().is_empty());
            let saved: SessionManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved, finished);
        }
        // A failed final outcome still needs the ordinary media inspection;
        // adopting it must not silently disable fragment recovery.
        let mut failed = final_manifest;
        failed.status = SessionStatus::Failed;
        failed.files.push(
            serde_json::from_value(serde_json::json!({
                "path":"missing.mov","source":"program","kind":"program",
                "width":320,"height":180,"fps":30,"audio":[],"frames":540
            }))
            .unwrap(),
        );
        {
            let directory = Directory::for_recording(&folder).unwrap();
            let mut initial = failed.clone();
            initial.status = SessionStatus::Recording;
            // Failed handoffs are in the initial plan, but removed from the
            // final file list when their writer never opened a media file.
            let mut unopened = initial.files[0].clone();
            unopened.path = "unopened.mov".into();
            unopened.source = "unopened".into();
            initial.files.push(unopened);
            write_manifest_in(&directory, name, &initial, true).unwrap();
            save_final_in(&directory, name, &failed).unwrap();
        }
        let recovered = recover_directory(&folder).unwrap();
        assert_eq!(recovered.sessions.len(), 1);
        assert_eq!(
            recovered.sessions[0].recording.status,
            SessionStatus::Failed
        );
        assert_eq!(recovered.sessions[0].recording.files[0].frames, Some(540));
        assert!(!recovered.sessions[0].recording.errors.is_empty());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
