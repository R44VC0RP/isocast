//! `doctor`: whether this Mac can record a setup, and what to fix.

use std::path::{Path, PathBuf};

use isocast_protocol::api::{Check, CheckStatus, Doctor};
use isocast_protocol::devices::{Permission, PermissionKind};
use isocast_protocol::{Codec, FilterKind, Setup, SourceKind, TransitionKind};

use crate::record::{expand_home, interrupted_note, interrupted_sessions};
use crate::{AudioTrackSpec, Platform, VideoTrackSpec, WriterSpec, space};

/// Recording time a disk should hold before it gets a warning.
const COMFORTABLE_HOURS: f64 = 1.0;

/// What a checked source delivers: its id, and a video format (width,
/// height, fps) or audio channels.
type Delivers<'a> = (&'a str, Option<(u32, u32, u32)>, Option<u32>);

/// Check this machine, and `setup` if given: permissions, hardware
/// encoders, the setup's devices and the recording disk.
pub fn doctor<P: Platform>(platform: &P, setup: Option<&Setup>, folder: Option<&Path>) -> Doctor {
    doctor_writing(platform, setup, folder, &|_| true)
}

/// [`doctor`], with the disk write test only where `may_write` allows (the
/// folder it writes in is the nearest one that exists): an engine skips it
/// for a client on another computer outside the folders it may use.
pub fn doctor_writing<P: Platform>(
    platform: &P,
    setup: Option<&Setup>,
    folder: Option<&Path>,
    may_write: &dyn Fn(&Path) -> bool,
) -> Doctor {
    let mut machine = platform.machine();
    let encoders = platform.video_encoders();
    machine.hardware_encoders = encoders.hardware;
    let mut checks = Vec::new();
    let check = |name: &str, status, detail: String, fix: Option<&str>| Check {
        name: name.into(),
        status,
        detail,
        fix: fix.map(Into::into),
    };

    // Permissions, and which ones the setup needs.
    let kinds: Vec<&SourceKind> = setup
        .map(|s| s.sources.iter().map(|source| &source.kind).collect())
        .unwrap_or_default();
    let uses = |f: fn(&SourceKind) -> bool| kinds.iter().any(|kind| f(kind));
    match platform.permissions() {
        Ok(report) => {
            // System and app audio use system audio recording where it's
            // granted, else screen recording; neither alone is required.
            let screen = uses(|k| {
                matches!(
                    k,
                    SourceKind::Display(_) | SourceKind::Window(_) | SourceKind::Application(_)
                )
            });
            let played = uses(|k| {
                matches!(
                    k,
                    SourceKind::ApplicationAudio(_) | SourceKind::SystemAudio(_)
                )
            });
            // Unknown counts: recording tries it rather than being blocked.
            let either = |kind| {
                matches!(
                    report.permissions.get(kind),
                    Permission::Granted | Permission::Unknown
                )
            };
            let played_missing = played
                && !either(PermissionKind::ScreenRecording)
                && !either(PermissionKind::SystemAudio);
            let camera = uses(|k| matches!(k, SourceKind::Camera(_)));
            let microphone = uses(|k| matches!(k, SourceKind::Microphone(_)));
            for detail in &report.details {
                let needed = match detail.permission {
                    PermissionKind::ScreenRecording => screen,
                    PermissionKind::Camera => camera,
                    PermissionKind::Microphone => microphone,
                    PermissionKind::SystemAudio => played_missing,
                };
                let name = format!("permission: {}", detail.permission.label());
                let fix = detail.next_step.as_deref();
                checks.push(match (detail.state, needed) {
                    (Permission::Granted, _) => {
                        check(&name, CheckStatus::Ok, "granted".into(), None)
                    }
                    (Permission::NotDetermined, _) => check(
                        &name,
                        CheckStatus::Warning,
                        isocast_protocol::platform_text::select_linux(
                            "not asked yet; macOS asks the first time it is needed",
                            "not checked yet; review Windows privacy access for desktop apps",
                            "not checked yet",
                        )
                        .into(),
                        fix,
                    ),
                    // Never a problem: recording still tries it.
                    (Permission::Unknown, _) => check(
                        &name,
                        CheckStatus::Warning,
                        "unknown; it can't be checked without using it, so recording tries it"
                            .into(),
                        fix,
                    ),
                    (other, true) => check(
                        &name,
                        CheckStatus::Problem,
                        format!("{} and the setup needs it", other.label()),
                        fix,
                    ),
                    (other, false) => check(
                        &name,
                        CheckStatus::Warning,
                        format!("{}; this setup doesn't need it", other.label()),
                        fix,
                    ),
                });
            }
        }
        Err(err) => checks.push(check(
            "devices",
            CheckStatus::Problem,
            format!("could not check permissions: {err:#}"),
            None,
        )),
    }

    // Hardware encoders.
    if let Some(budget) = platform.frame_budget() {
        let invariant = budget["invariant"].as_bool() == Some(true);
        let refused = budget["refused"]
            .as_array()
            .is_some_and(|counts| counts.iter().any(|n| n.as_u64().unwrap_or(0) != 0));
        checks.push(check(
            "frame budget",
            if !invariant { CheckStatus::Problem } else if refused { CheckStatus::Warning } else { CheckStatus::Ok },
            format!("distinct-buffer invariant {}; base reserves: production {}, recording {}, auxiliary {}, composition {}; refusals {} (since daemon start)",
                if invariant { "holds" } else { "FAILED" }, budget["production_reserve"], budget["recording_reserve"], budget["auxiliary_limit"], budget["composition_reserve"], budget["refused"]),
            refused.then_some("reduce auxiliary viewers/outputs; status.diagnostics.frame_budget identifies the exhausted domain"),
        ));
    }

    // Codecs this machine should have but can't use (Linux's OpenH264).
    for missing in platform.missing_codecs() {
        checks.push(check(
            &format!("codec: {}", missing.name),
            CheckStatus::Problem,
            missing.reason,
            Some(&missing.fix),
        ));
    }

    // Browser sources that would run degraded (Linux: no WebKit sandbox).
    if (setup.is_none() || uses(|kind| matches!(kind, SourceKind::Browser(_))))
        && let Some(warning) = platform.browser_warning()
    {
        checks.push(check(
            "browser sources",
            CheckStatus::Warning,
            warning.detail,
            Some(&warning.fix),
        ));
    }

    // Hardware encoders.
    let has = |name: &str| machine.hardware_encoders.iter().any(|e| e.contains(name));
    checks.push(check(
        "hardware encoders",
        if has("HEVC") {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        if machine.hardware_encoders.is_empty() && encoders.software.is_empty() {
            "none found; encoding falls back to the CPU".into()
        } else if machine.hardware_encoders.is_empty() {
            format!(
                "none found; encoding falls back to the CPU: {}",
                encoders.software.join(", ")
            )
        } else {
            machine.hardware_encoders.join(", ")
        },
        None,
    ));

    for reason in encoders.unavailable {
        checks.push(check(
            "hardware encoder unavailable",
            CheckStatus::Warning,
            reason,
            None,
        ));
    }
    checks.extend(platform.doctor_checks());

    let Some(setup) = setup else {
        checks.push(check(
            "setup",
            CheckStatus::Warning,
            "no setup given, so only this machine was checked".into(),
            Some("pass a setup file, or apply one to the live setup"),
        ));
        return Doctor { machine, checks };
    };

    let issues = setup.validate();
    if issues.is_empty() {
        checks.push(check("setup", CheckStatus::Ok, "valid".into(), None));
    } else {
        for issue in &issues {
            checks.push(check(
                &format!("setup: {}", issue.path),
                CheckStatus::Problem,
                issue.message.clone(),
                Some("fix the setup; `isocast setup check` lists every problem"),
            ));
        }
    }
    // Files in folders macOS guards: reading them waits for its prompt.
    let home = std::env::var("HOME").unwrap_or_default();
    let guarded = guarded_files(setup, &home);
    if !cfg!(windows) && !guarded.is_empty() {
        checks.push(check(
            "folder access",
            CheckStatus::Warning,
            format!(
                "the setup reads files in folders macOS guards, so their sources wait for its \
                 permission prompt the first time: {}",
                guarded.join(", ")
            ),
            Some(
                "allow access when macOS asks (System Settings → Privacy & Security → Files \
                 and Folders), or keep the files in another folder",
            ),
        ));
    }
    let codec = setup.recording.iso.encoder.codec;
    let hardware = match codec {
        Codec::Hevc => has("HEVC"),
        Codec::H264 => has("H.264"),
        _ => has("ProRes"),
    };
    if !hardware {
        checks.push(check(
            "encoder",
            CheckStatus::Warning,
            format!("{codec:?} has no hardware encoder here, so it uses the CPU"),
            Some("pick a codec listed under hardware encoders"),
        ));
    }
    // The daemon adds encoder capacity from the same profile-aware method
    // as `capacity` and `setup check` (the profile belongs to the daemon).

    // Every source the recording uses, resolved to a device: ISO files
    // and their audio, and the program's audio (a recording won't start
    // without them), then the cameras and screens the program's scenes
    // show (a missing one is only blank in the program).
    let iso = &setup.recording.iso;
    let program = &setup.recording.program;
    let iso_on = iso.enabled && !iso.sources.is_empty();
    let mut needed: Vec<&str> = Vec::new();
    if iso_on {
        needed.extend(
            iso.sources
                .iter()
                .chain(iso.listed_audio())
                .map(String::as_str),
        );
    }
    if program.enabled {
        needed.extend(program.audio.iter().map(String::as_str));
    }
    let mut shown: Vec<&str> = Vec::new();
    if program.enabled {
        for scene in &setup.scenes {
            shown.extend(scene.video_sources(&setup.scenes));
        }
    }
    let mut checked: Vec<&str> = Vec::new();
    let mut formats: Vec<Delivers> = Vec::new();
    for (id, required) in needed
        .iter()
        .map(|id| (*id, true))
        .chain(shown.iter().map(|id| (*id, false)))
    {
        if checked.contains(&id) {
            continue;
        }
        checked.push(id);
        let Some(source) = setup.source(id) else {
            continue;
        };
        let name = format!("source: {id}");
        match &source.kind {
            SourceKind::TestPattern(pattern) => {
                formats.push((id, Some((pattern.width, pattern.height, pattern.fps)), None));
                checks.push(check(&name, CheckStatus::Ok, "synthetic".into(), None));
                continue;
            }
            SourceKind::Tone(_) => {
                formats.push((id, None, Some(2)));
                checks.push(check(&name, CheckStatus::Ok, "synthetic".into(), None));
                continue;
            }
            SourceKind::Display(_)
            | SourceKind::Window(_)
            | SourceKind::Application(_)
            | SourceKind::ApplicationAudio(_)
            | SourceKind::Camera(_)
            | SourceKind::Microphone(_)
            | SourceKind::SystemAudio(_) => {}
            // Pictures, pages and media aren't devices to look for.
            _ => continue,
        }
        match platform.probe(&source.id, &source.kind, &setup.canvas) {
            Ok(Some(probe)) => {
                let detail = match (probe.video, probe.channels) {
                    (Some(f), _) => {
                        formats.push((id, Some((f.width, f.height, f.fps)), None));
                        format!("{}: {}×{} @ {} fps", probe.device, f.width, f.height, f.fps)
                    }
                    (None, Some(channels)) => {
                        formats.push((id, None, Some(channels)));
                        format!("{}: {channels} channel(s)", probe.device)
                    }
                    (None, None) => probe.device,
                };
                checks.push(check(&name, CheckStatus::Ok, detail, None));
            }
            Ok(None) => {}
            Err(err) if required => checks.push(check(
                &name,
                CheckStatus::Problem,
                format!("{err:#}; the recording needs it, so it won't start"),
                Some("connect the device, or change the source's device in the setup"),
            )),
            Err(err) => checks.push(check(
                &name,
                CheckStatus::Warning,
                format!("{err:#}; the program shows nothing in its place"),
                Some("connect the device, or change the source's device in the setup"),
            )),
        }
    }

    // The recording disk, at the files' highest likely data rate: each
    // ISO file with its audio tracks, and the program with its mix.
    let folder = folder
        .map(Path::to_path_buf)
        .unwrap_or_else(|| expand_home(&setup.recording.folder));
    let existing = nearest_existing(&folder);
    let channels = |ids: &[String]| -> Vec<AudioTrackSpec> {
        ids.iter()
            .filter_map(|id| formats.iter().find(|(f, ..)| f == id))
            .filter_map(|(_, _, channels)| *channels)
            .map(|channels| AudioTrackSpec { channels })
            .collect()
    };
    let file = |(width, height, fps), encoder: &isocast_protocol::Encoder, audio| {
        space::bytes_per_second(&WriterSpec {
            path: PathBuf::new(),
            container: encoder.container,
            video: VideoTrackSpec {
                width,
                height,
                fps,
                encoder: encoder.clone(),
            },
            audio,
            created: std::time::SystemTime::now(),
        })
    };
    let mut rate = 0.0;
    if iso_on {
        for id in &iso.sources {
            if let Some((_, Some(video), _)) = formats.iter().find(|(f, ..)| f == id) {
                let source = setup.source(id).expect("validated source");
                let own = formats
                    .iter()
                    .any(|(f, _, audio)| f == id && audio.is_some());
                let encoder = iso.encoder_for(source);
                rate += file(*video, &encoder, channels(&iso.audio_for(source, own)));
            }
        }
    }
    if program.enabled {
        let (width, height) = program.size(&setup.canvas);
        let mix = match program.audio.is_empty() {
            true => Vec::new(),
            false => vec![AudioTrackSpec { channels: 2 }],
        };
        rate += file((width, height, setup.canvas.fps), &program.encoder, mix);
    }
    let name = format!("disk: {}", folder.display());
    match platform.volume(&existing) {
        Ok(volume) => {
            let reserve = setup.recording.keep_free_gb * 1e9;
            let hours = (volume.free_bytes as f64 - reserve).max(0.0) / rate.max(1.0) / 3600.0;
            let fits = format!(
                "{} free ({}) holds about {hours:.1} h at up to {}/minute",
                space::human(volume.free_bytes as f64),
                volume.file_system,
                space::human(rate * 60.0)
            );
            checks.push(if hours < space::MIN_OPEN_ENDED_SECONDS / 3600.0 {
                check(
                    &name,
                    CheckStatus::Problem,
                    fits,
                    Some("free space or record to a bigger disk"),
                )
            } else if hours < COMFORTABLE_HOURS {
                check(
                    &name,
                    CheckStatus::Warning,
                    fits,
                    Some("free space or record to a bigger disk"),
                )
            } else {
                check(&name, CheckStatus::Ok, fits, None)
            });
            if volume.network {
                checks.push(check(
                    &format!("{name} network"),
                    CheckStatus::Warning,
                    format!("a network share ({})", volume.file_system),
                    Some("record to a local disk; a dropped connection can lose the files"),
                ));
            }
            if !may_write(&existing) {
                checks.push(check(
                    &format!("{name} write speed"),
                    CheckStatus::Warning,
                    format!(
                        "not tested: the test would write in {}, which isn't a folder this \
                         client may use",
                        existing.display()
                    ),
                    Some("run doctor on the engine's computer, or choose the folder there"),
                ));
            } else {
                match space::write_speed(&existing) {
                    Ok(speed) => {
                        let detail = format!(
                            "wrote {}/s in a test; the files may need up to {}/s",
                            space::human(speed),
                            space::human(rate)
                        );
                        checks.push(if speed < rate {
                            check(
                                &format!("{name} speed"),
                                CheckStatus::Problem,
                                detail,
                                Some("record to a faster disk"),
                            )
                        } else if speed < 2.0 * rate {
                            check(
                                &format!("{name} speed"),
                                CheckStatus::Warning,
                                detail,
                                Some("record to a faster disk"),
                            )
                        } else {
                            check(&format!("{name} speed"), CheckStatus::Ok, detail, None)
                        });
                    }
                    Err(err) => checks.push(check(
                        &format!("{name} speed"),
                        CheckStatus::Problem,
                        format!("could not write a test file: {err}"),
                        Some("check the folder exists and is writable"),
                    )),
                }
            }
        }
        Err(err) => checks.push(check(
            &name,
            CheckStatus::Problem,
            format!("{err:#}"),
            Some("check the folder"),
        )),
    }
    // Recordings there that a kill or crash left unfinished.
    // In it, and in its newest project folders.
    let interrupted: Vec<_> = isocast_protocol::project::recording_folders(&folder, 5)
        .iter()
        .flat_map(|folder| interrupted_sessions(folder, false))
        .collect();
    if let Some(note) = interrupted_note(&interrupted, false) {
        checks.push(check(
            &format!("recordings: {}", folder.display()),
            CheckStatus::Warning,
            note,
            Some("check those files; nothing else needs doing"),
        ));
    }

    Doctor { machine, checks }
}

/// The setup's files (images, media, slide shows, text, pages, masks,
/// LUTs, stingers) in the Desktop, Documents or Downloads folder, where
/// macOS asks before letting an app read them.
fn guarded_files(setup: &Setup, home: &str) -> Vec<String> {
    let mut files: Vec<&str> = Vec::new();
    for source in &setup.sources {
        match &source.kind {
            SourceKind::Image(image) => files.push(&image.path),
            SourceKind::Slideshow(show) => files.extend(show.files.iter().map(String::as_str)),
            SourceKind::Text(text) => files.extend(text.file.as_deref()),
            SourceKind::Media(media) => files.extend(media.files.iter().map(String::as_str)),
            SourceKind::Browser(page) => files.extend(page.file.as_deref()),
            _ => {}
        }
        for filter in &source.filters {
            match &filter.kind {
                FilterKind::ImageMask(mask) => files.push(&mask.path),
                FilterKind::ApplyLut(lut) => files.push(&lut.path),
                _ => {}
            }
        }
    }
    for (_, transition) in setup.transitions() {
        match &transition.kind {
            TransitionKind::LumaWipe(wipe) => files.extend(wipe.image.as_deref()),
            TransitionKind::Stinger(stinger) => files.push(&stinger.file),
            _ => {}
        }
    }
    let mut guarded = Vec::new();
    for file in files {
        let path = match file.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => file.to_string(),
        };
        let inside = ["Desktop", "Documents", "Downloads"]
            .iter()
            .any(|folder| path.starts_with(&format!("{home}/{folder}/")));
        if inside && !home.is_empty() && !guarded.contains(&file.to_string()) {
            guarded.push(file.to_string());
        }
    }
    guarded
}

/// `folder`, or its nearest parent that exists.
fn nearest_existing(folder: &Path) -> PathBuf {
    folder
        .ancestors()
        .find(|path| path.exists())
        .unwrap_or(Path::new("/"))
        .to_path_buf()
}
