//! Settings → Diagnostics: the engine's `doctor` checks grouped as a
//! person thinks of them (Capture, Encoding, Audio, Disk, Engine), each
//! with its status and what to do, plus Copy (the whole report as the CLI
//! prints it) and Report a Bug. The same report names the encoder the
//! engine really uses (Settings → Recording) and gives the Sources dock
//! its per-source warnings, alongside each running source's own
//! `warning` and a recording file's encoder change from status.
//!
//! Doctor runs when this page or Recording (its encoder line) is shown,
//! and on Refresh: it writes a test file and, on Windows, opens an
//! encoder. Each check shows one plain line (the common checks are worded
//! here, the rest use the engine's first sentence) and what to do; the
//! engine's own words wait behind Details, and Copy copies them all.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use isocast_protocol::api::{
    Check, CheckStatus, Doctor, DoctorParams, LiveFile, Method, SourceStatus,
};
use isocast_protocol::{Setup, SourceKind};
use slint::{ComponentHandle, SharedString};

use crate::app::App;
use crate::kit::model;
use crate::model as setup_model;
use crate::{DiagGroup, DiagRow, SettingsArea};

/// The groups, in the page's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Capture,
    Encoding,
    Audio,
    Disk,
    Engine,
}

impl Group {
    pub const ALL: [Group; 5] = [
        Group::Capture,
        Group::Encoding,
        Group::Audio,
        Group::Disk,
        Group::Engine,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Capture => "Capture",
            Group::Encoding => "Encoding",
            Group::Audio => "Audio",
            Group::Disk => "Disk",
            Group::Engine => "Engine",
        }
    }
}

/// Which group a doctor check belongs in, by its name. `audio_source`
/// says whether a source id (`source: ID`) is a sound-only source.
/// Unknown names go under Engine, so a new check is never hidden.
pub fn group_of(name: &str, audio_source: &dyn Fn(&str) -> bool) -> Group {
    let lower = name.to_ascii_lowercase();
    if let Some(id) = name.strip_prefix("source: ") {
        return if audio_source(id) {
            Group::Audio
        } else {
            Group::Capture
        };
    }
    if let Some(permission) = lower.strip_prefix("permission: ") {
        return if permission.contains("microphone") || permission.contains("audio") {
            Group::Audio
        } else {
            Group::Capture
        };
    }
    if lower.starts_with("disk: ") || lower.starts_with("recordings: ") {
        return Group::Disk;
    }
    if lower.contains("encoder") || lower.starts_with("codec: ") {
        return Group::Encoding;
    }
    if lower.contains("audio") || lower.contains("pipewire") {
        return Group::Audio;
    }
    match lower.as_str() {
        "devices"
        | "screencast portal"
        | "virtual camera"
        | "browser sources"
        | "folder access"
        | "media decoders (gstreamer)" => Group::Capture,
        "software graphics capacity" => Group::Encoding,
        _ => Group::Engine,
    }
}

/// Doctor's checks by group, in the page's order (empty groups left out),
/// each group's checks in doctor's order.
pub fn grouped<'a>(
    checks: &'a [Check],
    audio_source: &dyn Fn(&str) -> bool,
) -> Vec<(Group, Vec<&'a Check>)> {
    Group::ALL
        .iter()
        .map(|group| {
            let checks = checks
                .iter()
                .filter(|check| group_of(&check.name, audio_source) == *group)
                .collect::<Vec<_>>();
            (*group, checks)
        })
        .filter(|(_, checks)| !checks.is_empty())
        .collect()
}

/// Engine text without internal issue references: "#251: " starting a
/// sentence or clause is dropped.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('#') {
        let after = &rest[at + 1..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        let start_of_words = at == 0 || rest[..at].ends_with([' ', '(']);
        if digits > 0
            && start_of_words
            && let Some(tail) = after[digits..].strip_prefix(": ")
        {
            out.push_str(&rest[..at]);
            rest = tail;
            continue;
        }
        out.push_str(&rest[..=at]);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A check's name as a row title: "Source “Microphone”" for a source,
/// else its name with a capital ("Permission: camera").
pub fn title(name: &str, source_name: &dyn Fn(&str) -> Option<String>) -> String {
    if let Some(id) = name.strip_prefix("source: ") {
        let name = source_name(id).unwrap_or_else(|| id.to_string());
        return format!("Source “{name}”");
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The whole report as `isocast doctor` prints it.
pub fn text(doctor: &Doctor) -> String {
    let m = &doctor.machine;
    let mut out = format!(
        "{} · {:.0} GB · {} cores · {}\n",
        m.chip, m.memory_gb, m.cores, m.os
    );
    let (mut problems, mut warnings) = (0, 0);
    for check in &doctor.checks {
        let mark = match check.status {
            CheckStatus::Ok => "ok     ",
            CheckStatus::Warning => {
                warnings += 1;
                "warning"
            }
            CheckStatus::Problem => {
                problems += 1;
                "PROBLEM"
            }
        };
        out.push_str(&format!("{mark}  {}: {}\n", check.name, check.detail));
        if let Some(fix) = &check.fix {
            out.push_str(&format!("         fix: {fix}\n"));
        }
    }
    out.push_str(&format!("{problems} problem(s), {warnings} warning(s)\n"));
    out
}

/// The encoder the engine records with, as Settings → Recording names it,
/// and a note when it isn't the one that was wanted (a fallback) or none
/// works.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncoderLine {
    pub name: String,
    pub note: String,
}

/// An encoder provider's words ("NVENC", "Nvenc", "Media Foundation
/// software encoder (X)", "MediaFoundationSoftware(\"X\")") as a name.
fn provider(words: &str) -> Option<&'static str> {
    let squashed: String = words
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if squashed.starts_with("nvenc") || squashed.contains("nvidianvenc") {
        Some("NVIDIA NVENC (hardware)")
    } else if squashed.starts_with("mediafoundationhardware") {
        Some("Media Foundation (hardware)")
    } else if squashed.starts_with("mediafoundationsoftware") {
        Some("Media Foundation (software)")
    } else {
        None
    }
}

/// The encoder from doctor (Windows' self-test; Linux's NVENC and
/// OpenH264 checks), else VideoToolbox on a Mac engine, and, while
/// recording, the program file's actual encoder from status, which wins.
pub fn encoder(doctor: Option<&Doctor>, live: &[LiveFile], mac: bool) -> Option<EncoderLine> {
    let program = live
        .iter()
        .filter(|file| file.source == isocast_protocol::PROGRAM)
        .find_map(|file| file.encoder.as_ref());
    if let Some(encoder) = program
        && let Some(name) = provider(&encoder.backend)
    {
        return Some(EncoderLine {
            name: name.into(),
            note: encoder.reason.as_deref().map(plain).unwrap_or_default(),
        });
    }
    let mac = mac.then(|| EncoderLine {
        name: "Apple VideoToolbox (hardware)".into(),
        note: String::new(),
    });
    let Some(doctor) = doctor else { return mac };
    let named = |name: &str| doctor.checks.iter().find(|check| check.name == name);
    // Windows: the self-test opens the encoder recording would use.
    if let Some(test) = named("recording encoder self-test") {
        match test.status {
            CheckStatus::Ok => {
                let using = test.detail.rsplit_once("using ").map(|(_, rest)| rest);
                if let Some(using) = using
                    && let Some(name) = provider(using)
                {
                    let note = using
                        .split_once("; ")
                        .map(|(_, why)| {
                            let more = if name.contains("software") {
                                ", so recording uses more CPU"
                            } else {
                                ""
                            };
                            format!("Not the first choice: {}{more}.", plain(why))
                        })
                        .unwrap_or_default();
                    return Some(EncoderLine {
                        name: name.into(),
                        note,
                    });
                }
            }
            CheckStatus::Problem => {
                return Some(EncoderLine {
                    name: "No working encoder".into(),
                    note: plain(&test.detail),
                });
            }
            // Not tested (encoders busy): say nothing rather than guess.
            CheckStatus::Warning => return None,
        }
    }
    // Linux: NVENC when the driver has it, else OpenH264.
    if doctor
        .machine
        .hardware_encoders
        .iter()
        .any(|e| e.contains("NVENC"))
    {
        return Some(EncoderLine {
            name: "NVIDIA NVENC (hardware)".into(),
            note: String::new(),
        });
    }
    let Some(openh264) = named("codec: OpenH264") else {
        return mac;
    };
    let nvenc = doctor
        .checks
        .iter()
        .filter(|check| check.name == "hardware encoder unavailable")
        .find_map(|check| check.detail.strip_prefix("NVIDIA NVENC (H.264): "))
        .map(|why| format!("NVENC isn’t available: {}.", plain(why)));
    Some(match openh264.status {
        CheckStatus::Problem => EncoderLine {
            name: "OpenH264 (not installed)".into(),
            note: format!(
                "{} H.264 recording won’t start until it is installed.",
                openh264
                    .detail
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(':')
            ),
        },
        _ => EncoderLine {
            name: if openh264.detail.starts_with("Cisco") {
                "OpenH264 (Cisco, software)"
            } else if openh264.detail.starts_with("your system") {
                "OpenH264 (system, software)"
            } else {
                "OpenH264 (software)"
            }
            .into(),
            note: nvenc.unwrap_or_default(),
        },
    })
}

/// `text` as a sentence: a capital first and a full stop last.
pub fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches([';', ',', ':']);
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// The engine's first sentence of `text` (up to its first full stop, new
/// line or "; "), as a sentence.
pub fn first_sentence(text: &str) -> String {
    let text = plain(text);
    let end = [". ", "\n", "; "]
        .iter()
        .filter_map(|stop| text.find(stop))
        .min()
        .unwrap_or(text.len());
    sentence(&text[..end])
}

/// A source check's (or badge's) words as a sentence naming the source:
/// doctor's "could not open …; the program shows nothing in its place"
/// becomes "Camera isn't available, so the program shows nothing in its
/// place."
pub fn source_sentence(name: &str, detail: &str) -> String {
    let detail = plain(detail);
    if detail.ends_with("the program shows nothing in its place") {
        return format!("{name} isn’t available, so the program shows nothing in its place.");
    }
    if detail.ends_with("the recording needs it, so it won't start") {
        return format!("{name} isn’t available, and the recording needs it, so it won’t start.");
    }
    // A system-audio limitation after the device ("Speakers: 2
    // channel(s); …").
    let words = detail
        .split_once("; ")
        .map_or(detail.as_str(), |(_, rest)| rest);
    format!("{name}: {}", sentence(words))
}

/// What the Diagnostics page shows for a check: one plain line, what to do
/// (warnings and problems only), and the engine's own words behind
/// Details ("" when the plain line says it all).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plain {
    pub summary: String,
    pub fix: String,
    pub raw: String,
}

/// Doctor's check in plain words: the common checks mapped here, anything
/// else the engine's first sentence. `source_name` names a source by id;
/// `os` words the privacy-settings step.
pub fn plain_check(
    check: &Check,
    source_name: &dyn Fn(&str) -> Option<String>,
    os: isocast_protocol::hello::EngineOs,
) -> Plain {
    use isocast_protocol::hello::EngineOs;
    let ok = check.status == CheckStatus::Ok;
    let detail = plain(&check.detail);
    let engine_fix = || check.fix.as_deref().map(first_sentence).unwrap_or_default();
    let name = check.name.as_str();
    let lower = name.to_ascii_lowercase();
    let privacy = match os {
        EngineOs::Macos => "System Settings → Privacy & Security",
        EngineOs::Windows => "Settings → Privacy & security",
        _ => "your desktop’s privacy settings",
    };
    let (summary, fix): (String, String) = if let Some(kind) = lower.strip_prefix("permission: ") {
        if ok {
            ("Allowed.".into(), String::new())
        } else if detail.starts_with("not asked yet") || detail.starts_with("not checked yet") {
            (
                "Not asked yet; Isocast asks the first time it needs it.".into(),
                String::new(),
            )
        } else if detail.starts_with("unknown") {
            (
                "Can’t be checked without using it, so recording tries it.".into(),
                String::new(),
            )
        } else if detail.contains("the setup needs it") {
            (
                format!("Isocast doesn’t have {kind} permission, and this setup needs it."),
                format!("Turn on Isocast under {privacy}, then restart Isocast."),
            )
        } else {
            (
                format!("Isocast doesn’t have {kind} permission; this setup doesn’t need it."),
                format!("To use it later, turn on Isocast under {privacy}."),
            )
        }
    } else if let Some(id) = name.strip_prefix("source: ") {
        let source = source_name(id).unwrap_or_else(|| id.to_string());
        if ok {
            (
                if detail == "synthetic" {
                    "Built in, nothing to connect.".into()
                } else {
                    format!("Found: {detail}.")
                },
                String::new(),
            )
        } else {
            (
                source_sentence(&source, &detail),
                if check.fix.is_some() {
                    "Connect the device, or choose another one in the source’s Properties.".into()
                } else {
                    String::new()
                },
            )
        }
    } else if lower.starts_with("disk: ") {
        if lower.ends_with(" network") {
            (
                "The recording folder is on a network share.".into(),
                "Record to a local disk; a dropped connection can lose the files.".into(),
            )
        } else if lower.ends_with(" write speed") {
            (
                "Write speed wasn’t tested from this computer.".into(),
                engine_fix(),
            )
        } else if lower.ends_with(" speed") {
            let summary = match (
                detail.strip_prefix("wrote "),
                detail.split_once("may need up to "),
            ) {
                (Some(rest), Some((_, need))) if ok => format!(
                    "Fast enough: wrote {} a second in a test; recording needs up to {} a second.",
                    rest.split('/').next().unwrap_or_default(),
                    need.trim_end_matches("/s")
                ),
                _ if ok => "Fast enough.".into(),
                _ => first_sentence(&detail),
            };
            (
                summary,
                if ok {
                    String::new()
                } else {
                    "Record to a faster disk.".into()
                },
            )
        } else {
            // "38 GB free (apfs) holds about 0.8 h at up to 760 MB/minute"
            let free = detail.split(" free").next().unwrap_or_default();
            let hours = detail
                .split_once("holds about ")
                .and_then(|(_, rest)| rest.split(' ').next());
            let summary = match hours {
                Some(hours) => format!("{free} free, about {hours} hours of recording."),
                None => first_sentence(&detail),
            };
            (summary, if ok { String::new() } else { engine_fix() })
        }
    } else {
        match name {
            "ScreenCast portal" if detail.starts_with("X11 fallback available") => (
                "Using X11 capture; no desktop portal found.".into(),
                String::new(),
            ),
            "ScreenCast portal" if detail.starts_with("X11 capture active") => {
                ("Using X11 capture.".into(), String::new())
            }
            "ScreenCast portal" if ok => (
                "The desktop’s screen sharing portal is available.".into(),
                String::new(),
            ),
            "ScreenCast portal" => (
                "No screen sharing portal answered, so screens can’t be captured.".into(),
                engine_fix(),
            ),
            "media decoders (GStreamer)" if ok => (
                format!(
                    "Media files can play ({}).",
                    detail.split(';').next().unwrap_or("GStreamer")
                ),
                String::new(),
            ),
            "media decoders (GStreamer)" => (
                "Some media files can’t play: decoders are missing.".into(),
                engine_fix(),
            ),
            "virtual camera" if ok => ("Available.".into(), String::new()),
            "virtual camera" => (
                "The virtual camera isn’t available; recording is unaffected.".into(),
                "To use it, install v4l2loopback (the commands are under Details).".into(),
            ),
            "codec: OpenH264" if ok => (
                format!("Using {}.", detail.split(" (").next().unwrap_or(&detail)),
                String::new(),
            ),
            "codec: OpenH264" => (
                "OpenH264 isn’t installed, so H.264 can’t be recorded or previewed.".into(),
                "Install OpenH264 (the command is under Details), then Refresh.".into(),
            ),
            "hardware encoders" if detail.starts_with("none found") => (
                "No hardware encoder found, so recording uses the CPU.".into(),
                String::new(),
            ),
            "hardware encoders" => ("Hardware encoding is available.".into(), String::new()),
            "hardware encoder unavailable" => (
                match detail.split_once(": ") {
                    Some((encoder, _)) => format!("{encoder} isn’t available."),
                    None => first_sentence(&detail),
                },
                String::new(),
            ),
            "encoder" => (
                first_sentence(&detail),
                if os == EngineOs::Linux {
                    "Expected without an NVIDIA GPU; nothing needs doing.".into()
                } else {
                    "Choose a codec this computer encodes in hardware in Settings › Recording."
                        .into()
                },
            ),
            "recording encoder self-test" => match check.status {
                CheckStatus::Ok => (
                    match detail
                        .rsplit_once("using ")
                        .and_then(|(_, using)| provider(using))
                    {
                        Some(encoder) => format!("Recording encodes with {encoder}."),
                        None => "The encoder works.".into(),
                    },
                    String::new(),
                ),
                CheckStatus::Warning => (
                    "Not tested: the encoder is in use.".into(),
                    "Refresh when nothing is recording.".into(),
                ),
                CheckStatus::Problem => (
                    "The encoder couldn’t start, so recording will fail.".into(),
                    "Update the graphics driver, then Refresh.".into(),
                ),
            },
            "PipeWire server realtime" | "Isocast audio capture realtime" if ok => {
                ("Audio runs with real-time priority.".into(), String::new())
            }
            "PipeWire server realtime" | "Isocast audio capture realtime" => (
                "Audio may drop out: real-time priority isn’t confirmed.".into(),
                engine_fix(),
            ),
            "setup" if ok => ("The setup is valid.".into(), String::new()),
            "setup" => (
                "Only this computer was checked: there is no setup yet.".into(),
                String::new(),
            ),
            "recording video recovery" if ok => (
                "No video interruptions in the last take.".into(),
                String::new(),
            ),
            "recording video recovery" => (
                "Part of a take’s video was lost.".into(),
                "Details say which source or file and when.".into(),
            ),
            "recording metadata publication" => (
                "A take’s details couldn’t be saved yet; Isocast keeps retrying.".into(),
                engine_fix(),
            ),
            "folder access" => (
                "Some files are in folders macOS asks permission for.".into(),
                "Allow access when macOS asks.".into(),
            ),
            "frame budget" if ok => ("Frame buffers are within budget.".into(), String::new()),
            "devices" => ("Permissions couldn’t be checked.".into(), String::new()),
            _ if lower.starts_with("recordings: ") => (
                "Some recordings here were left unfinished by a crash.".into(),
                "Check those files; nothing else needs doing.".into(),
            ),
            _ if lower.starts_with("setup: ") => (
                format!("The setup has a problem: {}", sentence(&detail)),
                "Fix it in Settings or the setup file.".into(),
            ),
            _ => (
                first_sentence(&detail),
                if ok { String::new() } else { engine_fix() },
            ),
        }
    };
    let fix = if ok { String::new() } else { fix };
    let mut raw = check.detail.clone();
    if let Some(engine) = &check.fix {
        raw.push_str("\nfix: ");
        raw.push_str(engine);
    }
    // Details only when the engine said more than the plain line.
    let shown = format!("{summary} {fix}").to_lowercase();
    let raw = if raw.trim().len() <= summary.len() || shown.contains(&raw.trim().to_lowercase()) {
        String::new()
    } else {
        raw
    };
    Plain { summary, fix, raw }
}

/// What a source's warning badge says, as sentences naming the source
/// (`name`), or None: the running source's own warning (a system-audio
/// limitation), its system-audio mode when it isn't the plain one, its
/// recording file's encoder change, and doctor's warning about it.
pub fn source_warning(
    id: &str,
    name: &str,
    running: Option<&SourceStatus>,
    live: &[LiveFile],
    doctor: Option<&Doctor>,
) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut add = |line: String| {
        if !line.is_empty() && !lines.contains(&line) {
            lines.push(line);
        }
    };
    if let Some(run) = running {
        if let Some(mode) = run.capture_mode.as_deref()
            && let Some(words) = mode_words(mode)
        {
            add(format!("{name} {words}"));
        }
        if let Some(warning) = &run.warning {
            add(format!("{name}: {}", sentence(&plain(warning))));
        }
    }
    for file in live.iter().filter(|file| file.source == id) {
        if let Some(reason) = file.encoder.as_ref().and_then(|e| e.reason.as_deref()) {
            let reason = plain(reason);
            let reason = reason.strip_prefix(&format!("{id}: ")).unwrap_or(&reason);
            add(format!(
                "{name}’s ISO file uses another encoder than planned: {}",
                sentence(reason)
            ));
        }
    }
    let check_name = format!("source: {id}");
    if let Some(doctor) = doctor {
        for check in doctor
            .checks
            .iter()
            .filter(|check| check.name == check_name && check.status != CheckStatus::Ok)
        {
            // Doctor repeats a running source's own warning after its
            // device ("Speakers: 2 channel(s); …"): said once.
            let detail = plain(&check.detail);
            if !running
                .and_then(|run| run.warning.as_deref())
                .is_some_and(|warning| detail.contains(plain(warning).as_str()))
            {
                add(source_sentence(name, &detail));
            }
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// A system-audio capture mode that changes what is heard, in words after
/// the source's name; the plain modes (the whole output) say nothing.
fn mode_words(mode: &str) -> Option<&'static str> {
    match mode {
        "process-loopback-exclude" => Some(
            "captures system audio app by app, leaving out excluded apps and Isocast’s own monitoring.",
        ),
        "playback-links-exclude-own" => {
            Some("captures system audio app by app, leaving out Isocast’s own monitoring.")
        }
        _ => None,
    }
}

/// Whether source `id` of `setup` is sound only.
fn audio_only(setup: Option<&Setup>, id: &str) -> bool {
    setup
        .and_then(|setup| setup_model::find_source(setup, id))
        .is_some_and(|source| {
            matches!(
                source.kind,
                SourceKind::Microphone(_)
                    | SourceKind::SystemAudio(_)
                    | SourceKind::ApplicationAudio(_)
                    | SourceKind::Tone(_)
            )
        })
}

#[derive(Default)]
pub struct DiagnosticsPage {
    doctor: RefCell<Option<Doctor>>,
    busy: Cell<bool>,
    /// Snapshots show a report from a file: the engine's isn't read over it.
    sampled: Cell<bool>,
}

impl DiagnosticsPage {
    pub fn connect(app: &Rc<App>, area: &SettingsArea) {
        let weak = Rc::downgrade(app);
        area.on_diag_refresh(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.diagnostics.load(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_diag_copy(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.diagnostics.copy(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_diag_report(move || {
            if let Some(app) = weak.upgrade() {
                app.open_dialog("bug-report", "");
            }
        });
    }

    /// The last report, if any.
    pub fn doctor(&self) -> std::cell::Ref<'_, Option<Doctor>> {
        self.doctor.borrow()
    }

    /// Ask the engine's doctor again (one request at a time).
    pub fn load(&self, app: &Rc<App>) {
        if self.busy.get() || self.sampled.get() {
            return;
        }
        self.busy.set(true);
        if let Some(window) = app.window() {
            let area = window.global::<SettingsArea>();
            area.set_diag_busy(true);
            area.set_diag_copied(false);
        }
        let params = DoctorParams {
            setup: None,
            folder: None,
        };
        app.call_as::<Doctor>(Method::Doctor(Box::new(params)), |app, result| {
            let page = &app.settings.diagnostics;
            page.busy.set(false);
            if page.sampled.get() {
                return;
            }
            match result {
                Ok(doctor) => page.set(app, doctor, String::new()),
                Err(error) => {
                    let error = format!("Couldn’t run the checks: {}", plain(&error.message));
                    if let Some(window) = app.window() {
                        let area = window.global::<SettingsArea>();
                        area.set_diag_busy(false);
                        area.set_diag_error(error.into());
                    }
                }
            }
        });
    }

    /// Snapshots: show the doctor report in `path` (JSON, as `isocast
    /// doctor --json` prints it) instead of the engine's.
    pub fn sample(&self, app: &Rc<App>, path: &str) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let doctor: Doctor = serde_json::from_str(&text)?;
        self.sampled.set(true);
        self.set(app, doctor, String::new());
        Ok(())
    }

    fn set(&self, app: &Rc<App>, doctor: Doctor, error: String) {
        *self.doctor.borrow_mut() = Some(doctor);
        if let Some(window) = app.window() {
            let area = window.global::<SettingsArea>();
            area.set_diag_busy(false);
            area.set_diag_error(error.into());
        }
        self.show(app);
        app.settings.encoder_shown(app);
        super::sources::show_lists(app);
    }

    /// Put the report on the page.
    pub fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<SettingsArea>();
        let doctor = self.doctor.borrow();
        let Some(doctor) = doctor.as_ref() else {
            area.set_diag_groups(model(Vec::<DiagGroup>::new()));
            area.set_diag_summary(SharedString::new());
            return;
        };
        let s = app.session.borrow();
        let setup = s.setup.as_deref();
        let audio = |id: &str| audio_only(setup, id);
        let source_name = |id: &str| {
            setup
                .and_then(|setup| setup_model::find_source(setup, id))
                .map(|source| setup_model::source_name(source).to_string())
        };
        let groups = grouped(&doctor.checks, &audio)
            .into_iter()
            .map(|(group, checks)| DiagGroup {
                title: group.title().into(),
                rows: model(checks.into_iter().map(|check| {
                    let words = plain_check(check, &source_name, crate::engine::os());
                    DiagRow {
                        title: title(&check.name, &source_name).into(),
                        status: match check.status {
                            CheckStatus::Ok => "ok",
                            CheckStatus::Warning => "warning",
                            CheckStatus::Problem => "problem",
                        }
                        .into(),
                        summary: words.summary.into(),
                        fix: words.fix.into(),
                        raw: words.raw.into(),
                    }
                })),
            });
        area.set_diag_groups(model(groups));
        let count = |status| doctor.checks.iter().filter(|c| c.status == status).count();
        let (problems, warnings) = (count(CheckStatus::Problem), count(CheckStatus::Warning));
        let words = |n: usize, one: &str, many: &str| match n {
            0 => None,
            1 => Some(format!("1 {one}")),
            n => Some(format!("{n} {many}")),
        };
        let parts: Vec<String> = [
            words(problems, "problem", "problems"),
            words(warnings, "warning", "warnings"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let m = &doctor.machine;
        area.set_diag_summary(
            format!(
                "{} · {} · {:.0} GB · {}",
                if parts.is_empty() {
                    "Everything checked is fine".to_string()
                } else {
                    parts.join(", ")
                },
                m.chip,
                m.memory_gb,
                m.os
            )
            .into(),
        );
    }

    fn copy(&self, app: &Rc<App>) {
        let Some(text) = self.doctor.borrow().as_ref().map(text) else {
            return;
        };
        let text = isocast_diagnostics::redact::Redactor::default().text(&text);
        let copied = crate::platform::try_copy_text(&text);
        if let Some(window) = app.window() {
            let area = window.global::<SettingsArea>();
            area.set_diag_copied(copied);
            if !copied {
                area.set_diag_error("The clipboard isn’t available here.".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::api::Machine;
    use isocast_protocol::manifest::ManifestEncoder;

    fn check(name: &str, status: CheckStatus, detail: &str) -> Check {
        Check {
            name: name.into(),
            status,
            detail: detail.into(),
            fix: None,
        }
    }

    fn doctor(checks: Vec<Check>, hardware: &[&str]) -> Doctor {
        Doctor {
            windows_gpu: None,
            machine: Machine {
                chip: "test".into(),
                memory_gb: 8.0,
                cores: 4,
                os: "test".into(),
                hardware_encoders: hardware.iter().map(|s| s.to_string()).collect(),
            },
            checks,
        }
    }

    #[test]
    fn every_doctor_check_name_lands_in_its_group() {
        let audio = |id: &str| id == "mic" || id == "desktop";
        let cases = [
            ("permission: screen recording", Group::Capture),
            ("permission: camera", Group::Capture),
            ("permission: microphone", Group::Audio),
            ("permission: system audio recording", Group::Audio),
            ("devices", Group::Capture),
            ("ScreenCast portal", Group::Capture),
            ("virtual camera", Group::Capture),
            ("browser sources", Group::Capture),
            ("folder access", Group::Capture),
            ("media decoders (GStreamer)", Group::Capture),
            ("source: camera", Group::Capture),
            ("source: mic", Group::Audio),
            ("source: desktop", Group::Audio),
            ("hardware encoders", Group::Encoding),
            ("hardware encoder unavailable", Group::Encoding),
            ("encoder", Group::Encoding),
            ("recording encoder self-test", Group::Encoding),
            ("codec: OpenH264", Group::Encoding),
            ("encoder capacity", Group::Encoding),
            ("software graphics capacity", Group::Encoding),
            ("PipeWire server realtime", Group::Audio),
            ("Isocast audio capture realtime", Group::Audio),
            ("disk: /Users/me/Movies", Group::Disk),
            ("disk: /Users/me/Movies network", Group::Disk),
            ("disk: /Users/me/Movies speed", Group::Disk),
            ("disk: /Users/me/Movies write speed", Group::Disk),
            ("recordings: /Users/me/Movies", Group::Disk),
            ("setup", Group::Engine),
            ("setup: recording.iso.sources[0]", Group::Engine),
            ("frame budget", Group::Engine),
            ("recording video recovery", Group::Engine),
            ("recording metadata publication", Group::Engine),
            ("plugins: sandbox", Group::Engine),
            ("something new", Group::Engine),
        ];
        for (name, group) in cases {
            assert_eq!(group_of(name, &audio), group, "{name}");
        }
    }

    #[test]
    fn groups_keep_the_page_order_and_leave_out_empty_ones() {
        let checks = vec![
            check("setup", CheckStatus::Ok, "valid"),
            check("disk: /tmp", CheckStatus::Warning, "low"),
            check("permission: camera", CheckStatus::Ok, "granted"),
            check("hardware encoders", CheckStatus::Ok, "H.264"),
            check(
                "permission: screen recording",
                CheckStatus::Problem,
                "denied",
            ),
        ];
        let groups = grouped(&checks, &|_| false);
        let names: Vec<(Group, Vec<&str>)> = groups
            .iter()
            .map(|(g, c)| (*g, c.iter().map(|c| c.name.as_str()).collect()))
            .collect();
        assert_eq!(
            names,
            vec![
                (
                    Group::Capture,
                    vec!["permission: camera", "permission: screen recording"]
                ),
                (Group::Encoding, vec!["hardware encoders"]),
                (Group::Disk, vec!["disk: /tmp"]),
                (Group::Engine, vec!["setup"]),
            ]
        );
    }

    #[test]
    fn issue_numbers_leave_engine_text() {
        assert_eq!(
            plain("Speakers; #251: the exclude clock drifts"),
            "Speakers; the exclude clock drifts"
        );
        assert_eq!(plain("#251: drift"), "drift");
        assert_eq!(plain("track #2 of 3"), "track #2 of 3");
        assert_eq!(plain("colour #ff0000"), "colour #ff0000");
        assert_eq!(plain("C# rules"), "C# rules");
    }

    #[test]
    fn titles_name_sources_and_start_with_a_capital() {
        let names = |id: &str| (id == "mic").then(|| "Microphone".to_string());
        assert_eq!(title("source: mic", &names), "Source “Microphone”");
        assert_eq!(title("source: gone", &names), "Source “gone”");
        assert_eq!(title("permission: camera", &names), "Permission: camera");
        assert_eq!(title("ScreenCast portal", &names), "ScreenCast portal");
    }

    #[test]
    fn the_encoder_line_names_each_platforms_real_encoder() {
        let windows = |detail: &str| {
            doctor(
                vec![check(
                    "recording encoder self-test",
                    CheckStatus::Ok,
                    detail,
                )],
                &[],
            )
        };
        let nvenc = windows("640x360 at 30 fps: 3 validated H264 pictures, first IDR, using NVENC");
        assert_eq!(
            encoder(Some(&nvenc), &[], false),
            Some(EncoderLine {
                name: "NVIDIA NVENC (hardware)".into(),
                note: String::new()
            })
        );
        let fallback = windows(
            "640x360 at 30 fps: 3 validated H264 pictures, first IDR, using Media Foundation \
             software encoder (H264 Encoder MFT); NVENC session failed",
        );
        let line = encoder(Some(&fallback), &[], false).unwrap();
        assert_eq!(line.name, "Media Foundation (software)");
        assert_eq!(
            line.note,
            "Not the first choice: NVENC session failed, so recording uses more CPU."
        );
        let busy = doctor(
            vec![check(
                "recording encoder self-test",
                CheckStatus::Warning,
                "Not tested: hardware encoder sessions are in use",
            )],
            &[],
        );
        assert_eq!(encoder(Some(&busy), &[], false), None);

        let linux_nvenc = doctor(
            vec![check(
                "codec: OpenH264",
                CheckStatus::Ok,
                "Cisco's OpenH264 2.6.0 (/x)",
            )],
            &["NVIDIA NVENC (H.264)"],
        );
        assert_eq!(
            encoder(Some(&linux_nvenc), &[], false).unwrap().name,
            "NVIDIA NVENC (hardware)"
        );
        let system = doctor(
            vec![
                check(
                    "hardware encoder unavailable",
                    CheckStatus::Warning,
                    "NVIDIA NVENC (H.264): no driver",
                ),
                check(
                    "codec: OpenH264",
                    CheckStatus::Ok,
                    "your system's OpenH264 2.7.0",
                ),
            ],
            &[],
        );
        let line = encoder(Some(&system), &[], false).unwrap();
        assert_eq!(line.name, "OpenH264 (system, software)");
        assert_eq!(line.note, "NVENC isn’t available: no driver.");
        let missing = doctor(
            vec![check(
                "codec: OpenH264",
                CheckStatus::Problem,
                "No OpenH264 found.\ncould not load libopenh264.so.8",
            )],
            &[],
        );
        assert_eq!(
            encoder(Some(&missing), &[], false).unwrap().name,
            "OpenH264 (not installed)"
        );
        assert_eq!(
            encoder(None, &[], true).unwrap().name,
            "Apple VideoToolbox (hardware)"
        );
        assert_eq!(encoder(None, &[], false), None);

        // While recording, the program file's own encoder wins.
        let live = [live_file(
            "program",
            "MediaFoundationHardware(\"Intel\")",
            Some("program: NVENC failed; recording with Media Foundation hardware encoder (Intel)"),
        )];
        let line = encoder(Some(&nvenc), &live, false).unwrap();
        assert_eq!(line.name, "Media Foundation (hardware)");
        assert!(line.note.contains("NVENC failed"));
    }

    fn live_file(source: &str, backend: &str, reason: Option<&str>) -> LiveFile {
        LiveFile {
            source: source.into(),
            encoder: Some(ManifestEncoder {
                admission: "planned".into(),
                source: None,
                calibrated: false,
                key: None,
                planned_preset: None,
                applied_preset: None,
                backend: backend.into(),
                estimated_load: None,
                reason: reason.map(Into::into),
            }),
            codec: None,
            hardware_accelerated: None,
            output_active: None,
            frames: 0,
            dropped_frames: 0,
            audio_dropouts: 0,
            committed_seconds: None,
        }
    }

    #[test]
    fn a_source_badge_gathers_its_warnings() {
        let running: SourceStatus = serde_json::from_value(serde_json::json!({
            "source": "desktop",
            "state": "running",
            "capture_mode": "process-loopback-exclude",
            "warning": "#251: process loopback has no clock of its own",
        }))
        .unwrap();
        let report = doctor(
            vec![
                check(
                    "source: desktop",
                    CheckStatus::Warning,
                    "Speakers: 2 channel(s); #251: process loopback has no clock of its own",
                ),
                check(
                    "source: camera",
                    CheckStatus::Ok,
                    "FaceTime: 1280×720 @ 30 fps",
                ),
                check(
                    "source: facecam",
                    CheckStatus::Warning,
                    "no camera named \"Elgato Facecam\" is connected; the program shows nothing in its place",
                ),
            ],
            &[],
        );
        assert_eq!(
            source_warning(
                "desktop",
                "Desktop Audio",
                Some(&running),
                &[],
                Some(&report)
            )
            .unwrap(),
            "Desktop Audio captures system audio app by app, leaving out excluded apps and \
             Isocast’s own monitoring.\nDesktop Audio: Process loopback has no clock of its own."
        );
        assert_eq!(
            source_warning("camera", "Camera", None, &[], Some(&report)),
            None
        );
        assert_eq!(
            source_warning("desktop", "Desktop Audio", None, &[], Some(&report)).unwrap(),
            "Desktop Audio: Process loopback has no clock of its own."
        );
        assert_eq!(
            source_warning("facecam", "Facecam", None, &[], Some(&report)).unwrap(),
            "Facecam isn’t available, so the program shows nothing in its place."
        );
        let live = [live_file(
            "camera",
            "MediaFoundationSoftware(\"x\")",
            Some("camera: NVENC is full; recording with Media Foundation software encoder (x)"),
        )];
        assert_eq!(
            source_warning("camera", "Camera", None, &live, None).unwrap(),
            "Camera’s ISO file uses another encoder than planned: NVENC is full; recording with \
             Media Foundation software encoder (x)."
        );
        let plain_mode: SourceStatus = serde_json::from_value(serde_json::json!({
            "source": "desktop", "state": "running", "capture_mode": "endpoint-loopback",
        }))
        .unwrap();
        assert_eq!(
            source_warning("desktop", "Desktop Audio", Some(&plain_mode), &[], None),
            None
        );
    }

    fn with_fix(mut check: Check, fix: &str) -> Check {
        check.fix = Some(fix.into());
        check
    }

    #[test]
    fn checks_read_as_one_plain_line_with_the_engine_words_behind_details() {
        use isocast_protocol::hello::EngineOs;
        let names = |id: &str| (id == "camera").then(|| "Camera".to_string());
        let words = |c: &Check| plain_check(c, &names, EngineOs::Linux);

        let portal = words(&check(
            "ScreenCast portal",
            CheckStatus::Ok,
            "X11 fallback available: ScreenCast unavailable (Failed to connect to address \
             `unix:path=/tmp/x/missing`: No such file or directory (os error 2))",
        ));
        assert_eq!(
            portal.summary,
            "Using X11 capture; no desktop portal found."
        );
        assert_eq!(portal.fix, "");
        assert!(portal.raw.contains("unix:path=/tmp/x/missing"));

        let decoders = words(&check(
            "media decoders (GStreamer)",
            CheckStatus::Ok,
            "GStreamer 1.24.2; H.264: avdec_h264, openh264dec; HEVC: avdec_h265",
        ));
        assert_eq!(decoders.summary, "Media files can play (GStreamer 1.24.2).");
        assert!(decoders.raw.contains("avdec_h264"));

        let vcam = words(&with_fix(
            check(
                "virtual camera",
                CheckStatus::Warning,
                "Optional camera output unavailable; recording is unaffected",
            ),
            "No v4l2loopback camera device. Install v4l2loopback, then run: sudo modprobe v4l2loopback",
        ));
        assert_eq!(
            vcam.summary,
            "The virtual camera isn’t available; recording is unaffected."
        );
        assert!(vcam.fix.starts_with("To use it, install v4l2loopback"));
        assert!(vcam.raw.contains("sudo modprobe v4l2loopback"));

        let codec = words(&with_fix(
            check(
                "codec: OpenH264",
                CheckStatus::Problem,
                "No OpenH264 found.\ncould not load libopenh264.so.8",
            ),
            "~/.local/lib/isocast/install.sh --openh264",
        ));
        assert_eq!(
            codec.summary,
            "OpenH264 isn’t installed, so H.264 can’t be recorded or previewed."
        );
        assert!(codec.raw.contains("install.sh --openh264"));
        assert_eq!(
            words(&check(
                "codec: OpenH264",
                CheckStatus::Ok,
                "your system's OpenH264 2.7.0"
            ))
            .summary,
            "Using your system's OpenH264 2.7.0."
        );

        // A warning about a codec on Linux isn't a dead end.
        let encoder = words(&with_fix(
            check(
                "encoder",
                CheckStatus::Warning,
                "H264 has no hardware encoder here, so it uses the CPU",
            ),
            "pick a codec listed under hardware encoders",
        ));
        assert_eq!(
            encoder.fix,
            "Expected without an NVIDIA GPU; nothing needs doing."
        );

        let permission = plain_check(
            &check(
                "permission: camera",
                CheckStatus::Problem,
                "denied and the setup needs it",
            ),
            &names,
            EngineOs::Macos,
        );
        assert_eq!(
            permission.summary,
            "Isocast doesn’t have camera permission, and this setup needs it."
        );
        assert_eq!(
            permission.fix,
            "Turn on Isocast under System Settings → Privacy & Security, then restart Isocast."
        );

        // An OK check: its title and a short line, no Details for nothing.
        let granted = words(&check("permission: camera", CheckStatus::Ok, "granted"));
        assert_eq!(
            (granted.summary.as_str(), granted.raw.as_str()),
            ("Allowed.", "")
        );
        let synthetic = words(&check("source: camera", CheckStatus::Ok, "synthetic"));
        assert_eq!(synthetic.raw, "");

        let disk = words(&check(
            "disk: /tmp/rec",
            CheckStatus::Ok,
            "35.4 GB free (ext) holds about 2.5 h at up to 233 MB/minute",
        ));
        assert_eq!(disk.summary, "35.4 GB free, about 2.5 hours of recording.");
        let speed = words(&check(
            "disk: /tmp/rec speed",
            CheckStatus::Ok,
            "wrote 83 MB/s in a test; the files may need up to 4 MB/s",
        ));
        assert_eq!(
            speed.summary,
            "Fast enough: wrote 83 MB a second in a test; recording needs up to 4 MB a second."
        );

        let missing = words(&with_fix(
            check(
                "source: camera",
                CheckStatus::Warning,
                "no camera named \"Elgato Facecam\" is connected; the program shows nothing in its place",
            ),
            "connect the device, or change the source's device in the setup",
        ));
        assert_eq!(
            missing.summary,
            "Camera isn’t available, so the program shows nothing in its place."
        );
        assert!(missing.raw.contains("Elgato Facecam"));

        // Unmapped: the engine's first sentence, capitalised.
        let other = words(&with_fix(
            check(
                "plugins: sandbox",
                CheckStatus::Warning,
                "bubblewrap is missing; plugins run unsandboxed. More text",
            ),
            "install bubblewrap. Then restart",
        ));
        assert_eq!(other.summary, "Bubblewrap is missing.");
        assert_eq!(other.fix, "Install bubblewrap.");
        assert!(other.raw.contains("More text"));
    }
}
