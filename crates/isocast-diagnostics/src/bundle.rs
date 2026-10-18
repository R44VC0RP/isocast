//! Build the complete sanitized preview in memory before writing any archive.
use crate::{files, logs_folder, private_file, redact::Redactor};
use isocast_protocol::diagnostics::{
    BugReport, BugReportParams, ISSUE_REPOSITORY_URL, Logs, LogsParams,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_FILE: u64 = 4 << 20;
const MAX_BUNDLE: usize = 32 << 20;

pub fn logs(params: &LogsParams) -> Result<Logs, String> {
    let rank = |s: &str| match s {
        "debug" => 0,
        "info" => 1,
        "warn" => 2,
        "error" => 3,
        _ => -1,
    };
    let level = params.level.as_deref().unwrap_or("debug");
    if rank(level) < 0 {
        return Err("params.level: valid options: debug, info, warn, error".into());
    }
    let limit = params.limit.unwrap_or(200);
    if !(1..=1000).contains(&limit) {
        return Err("params.limit: valid range: 1..1000".into());
    }
    // Retire offsets for files removed by retention, too: a long-lived follower
    // must not accumulate an unbounded history of launch filenames.
    let mut cursor = BTreeMap::new();
    let mut records = Vec::new();
    let mut returned_bytes = 0;
    let mut full = false;
    for path in files(&logs_folder()) {
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let mut reader = BufReader::new(file);
        let at = params
            .cursor
            .as_ref()
            .and_then(|c| c.get(&name))
            .copied()
            .unwrap_or(0)
            .min(len);
        reader
            .seek(SeekFrom::Start(at))
            .map_err(|e| e.to_string())?;
        let mut offset = at;
        let mut line = String::new();
        loop {
            if params.cursor.is_some() && (records.len() >= limit || full) {
                break;
            }
            line.clear();
            let size = reader.read_line(&mut line).map_err(|e| e.to_string())?;
            if size == 0 || !line.ends_with('\n') {
                break;
            }
            if let Ok(mut record) = serde_json::from_str::<Value>(&line)
                && rank(record["level"].as_str().unwrap_or("info")) >= rank(level)
                && params
                    .component
                    .as_ref()
                    .is_none_or(|c| record["component"].as_str().unwrap_or("").contains(c))
            {
                if size > 4096 {
                    record = json!({"time":record["time"],"level":record["level"],"process":record["process"],"component":record["component"],
                        "message":record["message"].as_str().unwrap_or("").chars().take(300).collect::<String>(),
                        "fields":{"method":record["fields"]["request"]["method"],"original_bytes":size,"detail":"large record; read the report file for full contents"}});
                }
                let cost = record.to_string().len();
                if params.cursor.is_some() && returned_bytes + cost > 32768 && !records.is_empty() {
                    full = true;
                    break;
                }
                returned_bytes += cost;
                records.push(record);
                if params.cursor.is_none() && records.len() > limit * 2 {
                    records.sort_by(|a, b| a["time"].as_str().cmp(&b["time"].as_str()));
                    records.drain(..records.len() - limit);
                }
            }
            offset += size as u64;
        }
        cursor.insert(name, offset);
    }
    records.sort_by(|a, b| a["time"].as_str().cmp(&b["time"].as_str()));
    if records.len() > limit {
        records.drain(..records.len() - limit);
    }
    if params.cursor.is_none() {
        let mut bytes = records.iter().map(|r| r.to_string().len()).sum::<usize>();
        while bytes > 32768 && records.len() > 1 {
            bytes -= records.remove(0).to_string().len();
        }
    }
    Ok(Logs { records, cursor })
}

/// Snapshot is already typed/structured by the daemon; only selected manifests
/// and Isocast crash reports are opened. Never recurse into a media directory.
pub fn generate(
    params: &BugReportParams,
    snapshot: Value,
    manifest_folders: &[PathBuf],
    credentials: Vec<String>,
) -> Result<BugReport, String> {
    collect_report(params, snapshot, manifest_folders, credentials, true)
}

/// The daemon owns stream-key seeding, so a disconnected app must not export
/// historical free text (including bootstrap stderr). Keep typed launch
/// evidence and the user's description, through the normal preview/zip path.
pub fn disconnected(params: &BugReportParams) -> Result<BugReport, String> {
    if params.preview_id.is_some() {
        return from_preview(params);
    }
    let snapshot = json!({"environment":{"os":std::env::consts::OS}, "status":{"state":"engine unavailable"}, "build":{"version":env!("CARGO_PKG_VERSION"),"arch":std::env::consts::ARCH,"process":"app"}, "diagnostics":{"note":"The local engine could not be reached. Only launch markers are included; full logs and live checks need the engine."}});
    collect_report(
        params,
        snapshot,
        &[],
        std::env::var("ISOCAST_STREAM_KEY")
            .ok()
            .into_iter()
            .collect(),
        false,
    )
}

fn collect_report(
    params: &BugReportParams,
    snapshot: Value,
    manifest_folders: &[PathBuf],
    credentials: Vec<String>,
    live: bool,
) -> Result<BugReport, String> {
    check(params)?;
    let n = params.launches.unwrap_or(3);
    if !(1..=10).contains(&n) {
        return Err("params.launches: valid range: 1..10".into());
    }
    if params
        .describe
        .as_ref()
        .is_some_and(|d| d.chars().count() > 8000)
    {
        return Err("params.describe: maximum 8000 characters".into());
    }
    let plugin_logs = snapshot["plugins"]["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p["id"].as_str()?.to_owned(),
                PathBuf::from(p["log"].as_str()?),
            ))
        })
        .take(30)
        .collect::<Vec<_>>();
    let mut redactor = Redactor::report(params.hash_paths, &snapshot["devices"], credentials);
    let snapshot = redactor.value(&snapshot);
    let mut contents = BTreeMap::new();
    contents.insert(
        "environment.json".into(),
        serde_json::to_string_pretty(&snapshot["environment"]).unwrap(),
    );
    for key in [
        "status",
        "setup",
        "library",
        "profile",
        "plugins",
        "permissions",
        "devices",
        "build",
        "diagnostics",
    ] {
        if !live && snapshot.get(key).is_none() {
            continue;
        }
        contents.insert(
            format!("{key}.json"),
            serde_json::to_string_pretty(&snapshot[key]).unwrap(),
        );
    }
    if let Some(doctor) = snapshot.get("doctor") {
        contents.insert(
            "doctor.json".into(),
            serde_json::to_string_pretty(doctor).unwrap(),
        );
    }
    let mut omitted = Vec::new();
    for (id, path) in plugin_logs {
        collect(
            &path,
            format!("plugins/{id}.log"),
            &mut contents,
            &mut redactor,
            &mut omitted,
        );
        let previous = path.with_extension("previous");
        if previous.exists() {
            collect(
                &previous,
                format!("plugins/{id}-previous.log"),
                &mut contents,
                &mut redactor,
                &mut omitted,
            );
        }
    }
    let all = files(&logs_folder());
    let mut counts = HashMap::<String, usize>::new();
    let mut launches = HashSet::new();
    for path in all
        .iter()
        .rev()
        .filter(|p| p.extension().is_some_and(|e| e == "run"))
    {
        // Windows byte-range locks prevent even reading an active .run file.
        // Its JSONL segments aren't locked; identify them from our filename.
        #[cfg(windows)]
        if let Some((launch, process, pid)) = marker_name(path)
            && std::fs::File::open(path)
                .and_then(|mut f| f.read(&mut [0u8; 1]))
                .is_err_and(|e| e.raw_os_error() == Some(33))
        {
            let count = counts.entry(process.into()).or_default();
            if *count < n {
                if live {
                    launches.insert(launch.into());
                } else {
                    contents.insert(
                        format!("logs/{process}-{count}.json"),
                        json!({"process":process,"pid":pid,"marker_locked":true}).to_string(),
                    );
                }
                *count += 1;
            }
            continue;
        }
        if path.metadata().is_ok_and(|m| m.len() <= MAX_FILE)
            && let Ok(text) = std::fs::read_to_string(path)
            && let Some(first) = text.lines().next()
            && let Ok(value) = serde_json::from_str::<Value>(first)
            && let (Some(process), Some(launch)) =
                (value["process"].as_str(), value["launch"].as_str())
        {
            let count = counts.entry(process.into()).or_default();
            if *count < n {
                if live {
                    launches.insert(launch.to_owned());
                } else if report_process(process) {
                    let summary = json!({"process":process,"pid":value["pid"].as_u64(),"ready_marker":text.lines().any(|l|l=="{\"ready\":true}"),"clean_exit_marker":text.lines().any(|l|l=="{\"clean\":true}")});
                    contents.insert(format!("logs/{process}-{count}.json"), summary.to_string());
                }
                *count += 1;
            }
        }
    }
    let mut manifest_folders = manifest_folders.to_vec();
    if params.hash_paths {
        // Establish the report-wide pseudonym dictionary before redacting
        // newer prose that may refer to a now-disconnected device.
        for path in &all {
            if path.extension().is_some_and(|e| e == "jsonl")
                && path
                    .file_name()
                    .is_some_and(|n| launches.iter().any(|l| n.to_string_lossy().starts_with(l)))
                && path.metadata().is_ok_and(|m| m.len() <= MAX_FILE)
                && let Ok(text) = std::fs::read_to_string(path)
            {
                for line in text.lines() {
                    if let Ok(value) = serde_json::from_str::<Value>(line) {
                        redactor.include_devices(&value);
                    }
                }
            }
        }
    }
    // Newest segments win the bounded collection budget; reserve room for
    // crash reports and manifests instead of filling the zip with old logs.
    // Developer mode's segments (verbose records and traces) can fill the
    // report on their own: each launch's newest ones come along, up to one
    // full segment's worth, so a newest segment left tiny by a toggle or a
    // rotation brings the one before it too (#55).
    let mut developer: HashMap<String, u64> = HashMap::new();
    for path in all.iter().rev() {
        let name = path.file_name().unwrap().to_string_lossy();
        if launches.iter().any(|launch| name.starts_with(launch)) {
            if let Some(at) = name.find("-developer-") {
                let size = path.metadata().map_or(0, |m| m.len());
                let taken = developer.entry(name[..at].to_string()).or_insert(0);
                if *taken > 0 && *taken + size > MAX_FILE {
                    omitted.push(format!(
                        "logs/{name}: older developer-mode segment omitted (isocast logs reads it)"
                    ));
                    continue;
                }
                *taken += size.max(1);
            }
            if path.extension().is_some_and(|e| e == "jsonl")
                && contents.values().map(String::len).sum::<usize>() >= (24 << 20)
            {
                omitted.push(format!(
                    "logs/{name}: older log segment omitted to reserve crash/manifest space"
                ));
                continue;
            }
            if path.extension().is_some_and(|e| e == "jsonl")
                && path.metadata().is_ok_and(|m| m.len() <= MAX_FILE)
                && let Ok(text) = std::fs::read_to_string(path)
            {
                for line in text.lines() {
                    if let Ok(value) = serde_json::from_str::<Value>(line) {
                        let folder = value["fields"]["recording_report"]["folder"]
                            .as_str()
                            .or_else(|| value["fields"]["report"]["folder"].as_str());
                        if let Some(folder) = folder {
                            let folder = if let Some(rest) = folder.strip_prefix("~/") {
                                isocast_protocol::api::home_folder().join(rest)
                            } else {
                                PathBuf::from(folder)
                            };
                            if manifest_folders.len() < 20 && !manifest_folders.contains(&folder) {
                                manifest_folders.push(folder);
                            }
                        }
                    }
                }
            }
            collect(
                path,
                format!("logs/{name}"),
                &mut contents,
                &mut redactor,
                &mut omitted,
            );
        }
    }
    let home = isocast_protocol::api::home_folder();
    let bootstrap = isocast_protocol::api::daemon_file("log");
    if live && bootstrap.exists() {
        collect(
            &bootstrap,
            "bootstrap.log".into(),
            &mut contents,
            &mut redactor,
            &mut omitted,
        );
    }
    let mut macos_crashes = Vec::new();
    if live && cfg!(target_os = "macos") {
        macos_crashes = files(&home.join("Library/Logs/DiagnosticReports"));
        macos_crashes.extend(files(Path::new("/Library/Logs/DiagnosticReports")));
    }
    macos_crashes.sort_by_key(|p| p.metadata().ok().and_then(|m| m.modified().ok()));
    for path in macos_crashes
        .iter()
        .rev()
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_lowercase();
            CRASH_PREFIXES.iter().any(|p| name.starts_with(p))
        })
        .take(10)
    {
        collect(
            path,
            format!(
                "macos-crashes/{}-{}",
                if path.starts_with(&home) {
                    "user"
                } else {
                    "system"
                },
                path.file_name().unwrap().to_string_lossy()
            ),
            &mut contents,
            &mut redactor,
            &mut omitted,
        );
    }
    #[cfg(target_os = "linux")]
    collect_coredumps(&mut contents, &mut redactor, &mut omitted);
    let mut manifests = manifest_folders
        .iter()
        .flat_map(|dir| files(dir))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".session.json"))
        })
        .collect::<Vec<_>>();
    manifests.sort_by_key(|p| p.metadata().ok().and_then(|m| m.modified().ok()));
    manifests.dedup();
    for (i, path) in manifests.iter().rev().take(10).enumerate() {
        collect(
            path,
            format!("manifests/{i:02}.session.json"),
            &mut contents,
            &mut redactor,
            &mut omitted,
        );
    }
    let mut metrics = String::new();
    let mut last_metrics = Value::Null;
    let mut last_recording_metrics = Value::Null;
    let latest_abnormal = &snapshot["diagnostics"]["previous_abnormal"][0]["launch"];
    let mut last_abnormal_metrics = Value::Null;
    let mut last_panic_metrics = Value::Null;
    let last_panic = &snapshot["diagnostics"]["last_panic"];
    let mut warnings = Vec::new();
    for failure in snapshot["diagnostics"]["previous_logging_failure"]["events"]
        .as_array()
        .into_iter()
        .flatten()
    {
        warnings.push(json!({"time":failure["time"],"level":"warn","component":"logger","message":"diagnostic writes were unavailable","fields":failure}));
    }
    let mut crashes = Vec::new();
    let mut native_crashes = Vec::new();
    for (name, text) in &contents {
        if name.starts_with("macos-crashes/") && name.ends_with(".ips") {
            for value in serde_json::Deserializer::from_str(text)
                .into_iter::<Value>()
                .flatten()
            {
                if value["pid"].is_number() {
                    let mut summary = json!({"file":name,"pid":value["pid"],"process":value["procName"],"capture_time":value["captureTime"],"exception":value["exception"],"termination":value["termination"]});
                    if summary.to_string().len() > 2500 {
                        summary = json!({"file":name,"pid":value["pid"],"detail":"see native crash file"});
                    }
                    native_crashes.push(summary);
                }
            }
        }
        if name.starts_with("linux-crashes/") {
            let field = |key: &str| {
                text.lines()
                    .find_map(|line| line.trim_start().strip_prefix(key))
                    .map(str::trim)
            };
            native_crashes.push(json!({"file":name,"pid":field("PID:"),"process":field("Executable:"),"capture_time":field("Timestamp:"),"signal":field("Signal:")}));
        }
        if name.ends_with(".crash.json") {
            let mut crash =
                serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!({"panic":text}));
            if crash["launch"].is_null() {
                crash["launch"] = json!(
                    name.trim_start_matches("logs/")
                        .trim_end_matches(".crash.json")
                );
            }
            crashes.push(crash);
        }
        if !name.ends_with(".jsonl") {
            continue;
        }
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value["component"] == "metrics" {
                if value["fields"]["recording"].is_object() {
                    last_recording_metrics = json!({"time":value["time"],"launch":value["launch"],"recording":value["fields"]["recording"]});
                }
                metrics.push_str(line);
                metrics.push('\n');
                if last_metrics.is_null() || value["time"].as_str() > last_metrics["time"].as_str()
                {
                    last_metrics = value.clone();
                }
                if value["launch"] == *latest_abnormal {
                    last_abnormal_metrics = value.clone();
                }
                if value["launch"] == last_panic["launch"]
                    && value["time"].as_str() <= last_panic["time"].as_str()
                {
                    last_panic_metrics = value.clone();
                }
            }
            if matches!(value["level"].as_str(), Some("warn" | "error")) {
                warnings.push(value);
            }
        }
    }
    warnings.sort_by(|a, b| a["time"].as_str().cmp(&b["time"].as_str()));
    if warnings.len() > 20 {
        warnings.drain(..warnings.len() - 20);
    }
    // Cap individual triage entries, not the final JSON string: it must parse.
    for value in &mut warnings {
        if value.to_string().chars().count() > 1200 {
            let fields = &value["fields"];
            let cause = fields["response"]["error"]["message"]
                .as_str()
                .or_else(|| fields["error"].as_str())
                .or_else(|| fields["status"]["error"].as_str())
                .or_else(|| fields["report"]["errors"][0].as_str());
            *value = json!({"time":value["time"],"component":value["component"],"level":value["level"],
                "message":value["message"].as_str().unwrap_or("").chars().take(200).collect::<String>(),
                "method":fields["request"]["method"].as_str().map(|s|s.chars().take(100).collect::<String>()),
                "error":cause.map(|s|s.chars().take(600).collect::<String>()),"detail":"see logs in archive"});
        }
    }
    let crash = crashes.iter().find(|c| c["launch"] == *latest_abnormal);
    let signature = |c: &Value| json!({"launch":c["launch"],"panic":c["panic"].as_str().map(|s|s.chars().take(2000).collect::<String>()),"thread":c["thread"]});
    let triage = json!({"schema":1,"versions":snapshot["build"],"hardware":snapshot["environment"]["hardware"],
        "crash_signature":crash.map(signature),
        "retained_rust_crashes":crashes.iter().rev().take(3).map(signature).collect::<Vec<_>>(),
        "native_crashes":native_crashes.iter().rev().take(3).collect::<Vec<_>>(),
        "abnormal_cause":if latest_abnormal.is_null() {Value::Null} else if crash.is_some() {json!("Rust panic; see matching launch crash file")} else {json!("No clean-exit marker or matching Rust crash file; SIGKILL, power loss or a native crash are possible. See macos-crashes or linux-crashes and that launch's final log records.")},
        "last_error":snapshot["status"]["last_error"].as_str().map(|s|s.chars().take(2000).collect::<String>()),"last_warning_or_error":warnings.last(),
        "timeline":warnings,"previous_abnormal":snapshot["diagnostics"]["previous_abnormal"],
        "what_was_running":{"state":snapshot["status"]["state"],"scene":snapshot["status"]["scene"],"stream":snapshot["status"]["stream"]},
        "outputs":snapshot["status"]["live"],"last_recording":snapshot["status"]["last"]});
    let mut triage = triage;
    triage["machine_summary"] = json!(crate::submit::machine_summary(
        &contents["environment.json"]
    ));
    if !latest_abnormal.is_null() && !last_abnormal_metrics.is_null() {
        triage["what_was_running"] = incident_summary(&last_abnormal_metrics);
        triage["current_state"] = json!({"state":snapshot["status"]["state"],"stream":snapshot["status"]["stream"]["state"]});
    } else if !last_panic_metrics.is_null() {
        triage["what_was_running"] = incident_summary(&last_panic_metrics);
        triage["what_was_running"]["observed_at"] = json!("last sample before contained panic");
        triage["current_state"] = json!({"state":snapshot["status"]["state"],"stream":snapshot["status"]["stream"]["state"]});
    } else {
        triage["what_was_running"]["observed_at"] = json!("report time");
    }
    triage["interrupted_recordings"]=json!(snapshot["status"]["interrupted"].as_array().into_iter().flatten().take(10).map(|r|json!({"manifest":r["manifest"],"started_at":r["recording"]["started_at"],"duration_seconds":r["recording"]["duration_seconds"],"files":r["recording"]["files"].as_array().map(|v|v.iter().take(10).map(|f|json!({"path":f["path"],"source":f["source"],"duration_seconds":f["duration_seconds"],"frames":f["frames"]})).collect::<Vec<_>>())})).collect::<Vec<_>>());
    triage["logging_failures"] = snapshot["diagnostics"]["previous_logging_failure"].clone();
    if triage["what_was_running"]["stream"].to_string().len() > 4096 {
        triage["what_was_running"]["stream"] = json!({"state":snapshot["status"]["stream"]["state"],"detail":"see status.json for complete stream state"});
    }
    triage["last_logged_metrics"] = last_metrics;
    triage["last_recording_metrics"] = last_recording_metrics;
    triage["last_abnormal_metrics"] = incident_summary(&last_abnormal_metrics);
    if triage.to_string().chars().count() > 42000 {
        triage["last_recording"] = json!("see status.json");
        triage["outputs"] = json!("see status.json and metrics.jsonl");
        triage["hardware"] = json!("see environment.json");
        triage["last_logged_metrics"] = json!("see metrics.jsonl");
        triage["last_recording_metrics"] =
            json!("see metrics.jsonl for most recent recording counters");
        // The compact incident summary survives compaction.
    }
    let description = redactor.text(
        params
            .describe
            .as_deref()
            .unwrap_or("No description supplied."),
    );
    let description = description
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let guide = if live {
        "Review metrics.jsonl, logs/, status.json, manifests/ and crash reports in the report's zip."
    } else {
        "The engine was unavailable. This reduced app report includes build information and any available launch markers, not full logs or live checks."
    };
    let issue = format!(
        "# Isocast bug report\n\nState: {}. {} prior abnormal launch(es); {} recent warning/error entries.\n\n## Description\n\n{}\n\n## Agent triage\n\n```json\n{}\n```\n\n{guide} No media is included.\n",
        snapshot["status"]["state"],
        snapshot["diagnostics"]["previous_abnormal"]
            .as_array()
            .map_or(0, Vec::len),
        warnings.len(),
        description,
        serde_json::to_string(&triage).unwrap()
    );
    if issue.chars().count() >= 60000 {
        return Err(
            "issue body exceeds 60,000 characters; inspect report preview with fewer launches"
                .into(),
        );
    }
    contents.insert("ISSUE.md".into(), issue.clone());
    contents.insert("metrics.jsonl".into(), metrics);
    let redaction = json!({"policy":"credential fields/text and all HTTP/RTMP capability paths removed; serial fields omitted; home abbreviated; no environment dump or media. Text-source content and data-store values are retained unless credential-like. Default device names may contain owner names; strict mode pseudonymizes known device names/IDs/models and paths/titles with a fresh per-report HMAC key, never exported. Report credentials are seeded independently from configured Keychain accounts.", "hash_paths":params.hash_paths,"pseudonyms":"HMAC-SHA256 with independent random per-report key","counts":redactor.counts,"omitted":omitted});
    contents.insert(
        "REDACTION.json".into(),
        serde_json::to_string_pretty(&redaction).unwrap(),
    );
    build(params, contents, redaction)
}

/// Checks what every `bug_report` call must satisfy before collecting.
pub fn check(params: &BugReportParams) -> Result<(), String> {
    if params.submit && params.preview {
        return Err(
            "params.submit: preview first (preview: true), then submit with its preview_id".into(),
        );
    }
    if params.preview_id.is_some() && !params.submit {
        return Err("params.preview_id: only used with submit: true".into());
    }
    Ok(())
}

/// `submit` with a `preview_id`: write and send exactly that preview.
pub fn from_preview(params: &BugReportParams) -> Result<BugReport, String> {
    check(params)?;
    let id = params.preview_id.as_deref().unwrap_or_default();
    let contents = crate::preview::contents(id)?;
    let redaction = contents
        .get("REDACTION.json")
        .and_then(|r| serde_json::from_str(r).ok())
        .unwrap_or(Value::Null);
    build(params, contents, redaction)
}

fn build(
    params: &BugReportParams,
    contents: BTreeMap<String, String>,
    redaction: Value,
) -> Result<BugReport, String> {
    let names = contents.keys().cloned().collect();
    let mut report = BugReport {
        preview_id: None,
        file_sizes: contents.iter().map(|(n, s)| (n.clone(), s.len())).collect(),
        archive: None,
        issue_body: contents.get("ISSUE.md").cloned().unwrap_or_default(),
        repository_url: ISSUE_REPOSITORY_URL.map(String::from),
        files: names,
        redaction,
        submitted: None,
        submit_error: None,
    };
    if params.preview {
        report.preview_id = Some(crate::preview::save(contents)?);
        return Ok(report);
    }
    let home = isocast_protocol::api::home_folder();
    let out = params.out.clone().unwrap_or_else(default_out);
    let out = out
        .strip_prefix("~")
        .map_or_else(|_| out.clone(), |rest| home.join(rest));
    if !out.is_absolute() {
        return Err("params.out: use an absolute path or ~/...".into());
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("params.out: {e}"))?;
    let name = format!(
        "isocast-report-{}-{}.zip",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        std::process::id()
    );
    let path = out.join(name);
    let file = private_file(&path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for (name, text) in &contents {
        archive
            .start_file(name, options)
            .map_err(|e| e.to_string())?;
        archive
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    archive
        .finish()
        .map_err(|e| e.to_string())?
        .sync_all()
        .map_err(|e| e.to_string())?;
    report.archive = Some(path);
    if params.submit {
        crate::submit::send(&mut report, &contents);
    }
    Ok(report)
}

fn incident_summary(record: &Value) -> Value {
    if record.is_null() {
        return Value::Null;
    }
    let fields = &record["fields"];
    json!({"observed_at":"last sample before unclean exit","time":record["time"],"launch":record["launch"],
        "recording_active":fields["recording"].is_object(),"paused":fields["recording"]["paused"],"recording_elapsed_seconds":fields["recording"]["elapsed_seconds"],
        "outputs":fields["recording"]["files"],"stream_state":fields["stream"]["state"],"stream_elapsed_seconds":fields["stream"]["elapsed_seconds"],
        "stream_dropped_frames":fields["stream"]["dropped_frames"],"stream_audio_dropouts":fields["stream"]["audio_dropouts"],"process":fields["system"]["process"]})
}

/// File-name prefixes of Isocast's own crash reports.
const CRASH_PREFIXES: [&str; 6] = [
    "isocastd-",
    "isocast-",
    "isocast-app-",
    "isocast-ui-",
    "isocast_",
    "isocastd_",
];

/// `coredumpctl info` fields that identify the person or machine (user and
/// host names, machine and boot ids, session units), left out of reports.
#[cfg(target_os = "linux")]
const COREDUMP_PRIVATE: [&str; 11] = [
    "UID:",
    "GID:",
    "Owner UID:",
    "Hostname:",
    "Machine ID:",
    "Boot ID:",
    "Storage:",
    "Control Group:",
    "Unit:",
    "User Unit:",
    "Slice:",
];

/// The newest ten of this user's systemd-coredump records for Isocast
/// executables, as `coredumpctl info` prints them (with a backtrace when
/// symbols are available), under `linux-crashes/`. Nothing when
/// `coredumpctl` is missing or fails.
#[cfg(target_os = "linux")]
fn collect_coredumps(
    contents: &mut BTreeMap<String, String>,
    redactor: &mut Redactor,
    omitted: &mut Vec<String>,
) {
    use std::process::Command;
    let run = |args: &[&str]| {
        Command::new("coredumpctl")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(list) = run(&["list", "--no-pager", "--json=short"]) else {
        return;
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&list) else {
        return;
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = u64::from(unsafe { libc::getuid() });
    let mut ours = entries
        .iter()
        .filter_map(|entry| {
            let exe = Path::new(entry["exe"].as_str()?).file_name()?;
            let exe = exe.to_string_lossy().to_lowercase();
            let name = format!("{exe}-");
            (entry["uid"].as_u64() == Some(uid)
                && CRASH_PREFIXES.iter().any(|p| name.starts_with(p)))
            .then_some((entry["time"].as_u64()?, entry["pid"].as_u64()?, exe))
        })
        .collect::<Vec<_>>();
    ours.sort();
    for (time, pid, exe) in ours.into_iter().rev().take(10) {
        let name = format!("linux-crashes/{time}-{pid}-{exe}.txt");
        let current: usize = contents.values().map(String::len).sum();
        let Some(info) = run(&["info", "--no-pager", &pid.to_string()]) else {
            omitted.push(format!("{name}: coredumpctl info failed"));
            continue;
        };
        if info.len() as u64 > MAX_FILE || current.saturating_add(info.len()) > MAX_BUNDLE {
            omitted.push(format!("{name}: size/type limit"));
            continue;
        }
        contents.insert(name, coredump_text(&info, redactor));
    }
}

/// `coredumpctl info` output without [`COREDUMP_PRIVATE`] lines, redacted.
#[cfg(target_os = "linux")]
fn coredump_text(info: &str, redactor: &mut Redactor) -> String {
    info.lines()
        .filter(|line| {
            let line = line.trim_start();
            !COREDUMP_PRIVATE.iter().any(|key| line.starts_with(key))
        })
        .map(|line| redactor.text(line))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn collect(
    path: &Path,
    name: String,
    contents: &mut BTreeMap<String, String>,
    redactor: &mut Redactor,
    omitted: &mut Vec<String>,
) {
    let current: usize = contents.values().map(String::len).sum();
    let Ok(meta) = path.symlink_metadata() else {
        return;
    };
    if !meta.is_file()
        || meta.len() > MAX_FILE
        || current.saturating_add(meta.len() as usize) > MAX_BUNDLE
    {
        omitted.push(format!("{name}: size/type limit"));
        return;
    }
    let Ok(file) = std::fs::File::open(path) else {
        omitted.push(format!("{name}: unreadable"));
        return;
    };
    let mut text = String::new();
    if file.take(MAX_FILE + 1).read_to_string(&mut text).is_err() || text.len() as u64 > MAX_FILE {
        omitted.push(format!("{name}: unreadable or too large"));
        return;
    }
    let ips = if name.ends_with(".ips") {
        serde_json::Deserializer::from_str(&text)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>()
            .ok()
    } else {
        None
    };
    let safe = if let Some(values) = ips {
        values
            .iter()
            .map(|v| serde_json::to_string_pretty(&redactor.value(v)).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    } else if let Ok(value) = serde_json::from_str::<Value>(&text) {
        serde_json::to_string_pretty(&redactor.value(&value)).unwrap()
    } else {
        text.lines()
            .map(|line| match serde_json::from_str::<Value>(line) {
                Ok(value) => redactor.value(&value).to_string(),
                Err(_) => redactor.text(line),
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    contents.insert(name, safe);
}

#[cfg(any(windows, test))]
fn marker_name(path: &Path) -> Option<(&str, &str, u32)> {
    let launch = path.file_stem()?.to_str()?;
    let mut parts = launch.splitn(3, '-');
    chrono::NaiveDateTime::parse_from_str(parts.next()?, "%Y%m%dT%H%M%S%.9fZ").ok()?;
    let pid = parts.next()?.parse::<u32>().ok()?;
    let process = parts.next()?;
    report_process(process).then_some((launch, process, pid))
}

fn report_process(process: &str) -> bool {
    matches!(
        process,
        "isocastd" | "isocast" | "isocast-ui" | "isocast-app"
    )
}

/// Where a report goes when `out` isn't given: beside an isolated engine's
/// logs, else `~/Downloads/Isocast Reports`.
pub fn default_out() -> PathBuf {
    if std::env::var_os("ISOCAST_SOCKET").is_some() {
        logs_folder().with_extension("reports")
    } else {
        isocast_protocol::api::home_folder().join("Downloads/Isocast Reports")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn locked_launch_names_match_only_our_processes() {
        let file = std::path::Path::new("20261101T035454.655409600Z-6700-isocast-ui.run");
        assert_eq!(
            super::marker_name(file).map(|(_, name, pid)| (name, pid)),
            Some(("isocast-ui", 6700))
        );
        assert!(
            super::marker_name(std::path::Path::new(
                "20261101T035454.655409600Z-6700-private-app.run"
            ))
            .is_none()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn coredump_text_leaves_out_identifying_fields() {
        let info = "           PID: 4242 (isocastd)\n           UID: 1000 (mat)\n      Hostname: tanner\n    Machine ID: 0123abcd\n       Boot ID: 4567ef01\n        Signal: 6 (ABRT)\n     Executable: /usr/bin/isocastd\n Control Group: /user.slice/user-1000.slice\n          Unit: user@1000.service\n     User Unit: app.slice\n         Slice: user-1000.slice\n       Storage: /var/lib/systemd/coredump/core.isocastd.zst (present)\n";
        let text = coredump_text(info, &mut Redactor::default());
        assert!(text.contains("PID: 4242 (isocastd)"));
        assert!(text.contains("Signal: 6 (ABRT)"));
        assert!(text.contains("Executable: /usr/bin/isocastd"));
        for private in [
            "UID:",
            "Hostname:",
            "tanner",
            "Machine ID:",
            "Boot ID:",
            "Control Group:",
            "Unit:",
            "Slice:",
            "Storage:",
        ] {
            assert!(!text.contains(private), "{private} left in {text}");
        }
    }
}
