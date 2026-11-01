//! Apps that have opened audio, for `application_audio.app`. Read-only: the
//! audio session managers of the active render endpoints are listed, and
//! each session's process is opened only to read its executable path. No
//! stream is created or started and no endpoint or session setting is
//! written. COM objects never leave the thread that made them.
//!
//! An app's id is its full executable path: stable across runs, unlike the
//! process id. Several processes of one app (browser tabs, helpers) share
//! the id and are listed per process.

use std::collections::BTreeMap;
use std::path::Path;
use std::thread;

use anyhow::{Context, Result, anyhow, bail, ensure};
use isocast_protocol::devices::AudioApp;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2, IMMDeviceEnumerator,
    MMDeviceEnumerator, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{HRESULT, Interface, Owned, PWSTR};

use crate::native::Apartment;

/// Bounds on everything read across the FFI boundary: endpoints, sessions
/// in total, and UTF-16 units per string (also the longest Win32 path).
const MAX_ENTRIES: usize = 4096;
const MAX_TEXT: usize = 32_768;
/// Bound on the candidates listed in one error message.
const MAX_CHOICES: usize = 40;
/// AUDCLNT_S_NO_SINGLE_PROCESS (success): the session spans processes and
/// the reported id is its first process. Not exported by windows 0.62.
const NO_SINGLE_PROCESS: HRESULT = HRESULT(0x0889_000D);

/// List apps with audio sessions on a fresh MTA worker, joined before
/// returning.
pub(crate) fn audio_apps() -> Result<Vec<AudioApp>> {
    thread::Builder::new()
        .name("Windows audio apps".into())
        .spawn(|| {
            let _apartment = Apartment::new().map_err(|error| format!("{error:#}"))?;
            // Native error-info interfaces are formatted and dropped here,
            // inside the apartment that made them.
            current_apps(true).map_err(|error| format!("Windows audio apps: {error:#}"))
        })?
        .join()
        .map_err(|_| anyhow!("Windows audio app enumeration panicked"))?
        .map_err(anyhow::Error::msg)
}

/// List apps with audio sessions. The caller's thread must already be in
/// the MTA. Sessions that end or cannot be read while listing (process
/// exited, protected process, endpoint removed) are skipped with a
/// diagnostic; failing to list endpoints at all, or every active endpoint
/// refusing its sessions, is an error.
pub(crate) fn current_apps(report: bool) -> Result<Vec<AudioApp>> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("create the audio endpoint enumerator")?;
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .context("list active audio outputs")?;
    let count = unsafe { collection.GetCount() }.context("count active audio outputs")?;
    let count = count as usize;
    if report && count > MAX_ENTRIES {
        eprintln!(
            "Windows audio apps: listing the first {MAX_ENTRIES} of {count} audio outputs only"
        );
    }
    let own = std::process::id();
    let mut buffer = vec![0u16; MAX_TEXT];
    let mut entries = Vec::new();
    let mut budget = MAX_ENTRIES;
    let mut listed = 0usize;
    let mut first_error = None;
    for index in 0..count.min(MAX_ENTRIES) as u32 {
        let sessions = (|| -> Result<(IAudioSessionEnumerator, i32)> {
            let device = unsafe { collection.Item(index) }?;
            let manager: IAudioSessionManager2 =
                unsafe { device.Activate(CLSCTX_ALL, None) }.context("open the session manager")?;
            let sessions = unsafe { manager.GetSessionEnumerator() }.context("list sessions")?;
            let count = unsafe { sessions.GetCount() }.context("count sessions")?;
            Ok((sessions, count))
        })();
        let (sessions, count) = match sessions {
            Ok(sessions) => sessions,
            Err(error) => {
                if report {
                    eprintln!("Windows audio apps: skipping audio output {index}: {error:#}");
                }
                first_error.get_or_insert(error);
                continue;
            }
        };
        listed += 1;
        let count = usize::try_from(count).unwrap_or(0);
        if report && count > budget {
            eprintln!(
                "Windows audio apps: session limit {MAX_ENTRIES} reached; later sessions are \
                 not listed"
            );
        }
        for session in 0..count.min(budget) as i32 {
            budget -= 1;
            let read = unsafe { sessions.GetSession(session) }
                .context("open session")
                .and_then(|control| read_session(&control, own, &mut buffer, report));
            match read {
                Ok(Some(session)) => merge(&mut entries, session),
                Ok(None) => {}
                Err(error) if report => eprintln!(
                    "Windows audio apps: skipping session {session} of audio output {index}: \
                     {error:#}"
                ),
                Err(_) => {}
            }
        }
    }
    if listed == 0
        && let Some(error) = first_error
    {
        return Err(error.context("no active audio output could list its sessions"));
    }
    Ok(finish(entries))
}

/// One readable app session; `None` for sessions that are not an app's:
/// system sounds, expired, this process, or without a process.
struct Session {
    pid: u32,
    path: String,
    display: Option<String>,
    playing: bool,
}

fn read_session(
    control: &IAudioSessionControl,
    own: u32,
    buffer: &mut [u16],
    report: bool,
) -> Result<Option<Session>> {
    let state = unsafe { control.GetState() }.context("read session state")?;
    if state == AudioSessionStateExpired {
        return Ok(None);
    }
    let control: IAudioSessionControl2 = control.cast().context("open session process")?;
    if unsafe { control.IsSystemSoundsSession() } == S_OK {
        return Ok(None);
    }
    let mut pid = 0u32;
    // Called through the vtable to see AUDCLNT_S_NO_SINGLE_PROCESS, which
    // the wrapper folds into Ok. Such a session is listed under its first
    // process.
    let result = unsafe {
        (Interface::vtable(&control).GetProcessId)(Interface::as_raw(&control), &mut pid)
    };
    result.ok().context("read session process id")?;
    if report && result == NO_SINGLE_PROCESS {
        eprintln!("Windows audio apps: a session of process {pid} spans several processes");
    }
    if pid == 0 || pid == own {
        return Ok(None);
    }
    let path = process_path(pid, buffer)
        .with_context(|| format!("read the executable of process {pid}"))?;
    let display = unsafe { control.GetDisplayName() }
        .ok()
        .and_then(|text| unsafe { take_string(text) })
        .and_then(|name| display_name(&name));
    Ok(Some(Session {
        pid,
        path,
        display,
        playing: state == AudioSessionStateActive,
    }))
}

/// The process's full Win32 executable path. Fails for exited and some
/// protected processes.
fn process_path(pid: u32, buffer: &mut [u16]) -> Result<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?;
    // SAFETY: a newly opened handle, closed when `process` drops.
    let process = unsafe { Owned::new(process) };
    let mut length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    unsafe {
        QueryFullProcessImageNameW(
            *process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }?;
    let length = (length as usize).min(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..length]);
    ensure!(
        !path.is_empty() && !path.contains('\0'),
        "Windows reported an empty executable path"
    );
    Ok(path)
}

/// Take ownership of a CoTaskMem string, reading at most MAX_TEXT units.
/// Longer strings are refused rather than truncated.
unsafe fn take_string(text: PWSTR) -> Option<String> {
    struct Free(PWSTR);
    impl Drop for Free {
        fn drop(&mut self) {
            unsafe { CoTaskMemFree(Some(self.0.0.cast_const().cast())) };
        }
    }
    if text.is_null() {
        return None;
    }
    let text = Free(text);
    let mut length = 0;
    // SAFETY: COM returns a NUL-terminated string; the scan stops at the
    // terminator or the bound, whichever comes first.
    while length < MAX_TEXT && unsafe { *text.0.0.add(length) } != 0 {
        length += 1;
    }
    (length < MAX_TEXT)
        .then(|| String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text.0.0, length) }))
}

/// A session's own display name, if it set a usable one. Indirect resource
/// strings (`@dll,-id`) are not loaded: that would load another module.
fn display_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty() && !name.starts_with('@') && !name.chars().any(char::is_control))
        .then(|| name.to_string())
}

/// `C:\…\Spotify.exe` → `Spotify.exe`.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// `Spotify.exe` → `Spotify`.
fn stem(binary: &str) -> &str {
    match binary.len().checked_sub(4) {
        Some(at)
            if at > 0
                && binary
                    .get(at..)
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(".exe")) =>
        {
            &binary[..at]
        }
        _ => binary,
    }
}

struct Entry {
    app: AudioApp,
    /// The name came from a session, not the executable.
    named: bool,
}

/// One entry per process: its sessions on every endpoint merge, playing if
/// any plays, named by the first session that set a display name.
fn merge(entries: &mut Vec<Entry>, session: Session) {
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.app.pid == session.pid && entry.app.id == session.path)
    {
        entry.app.playing |= session.playing;
        if !entry.named
            && let Some(display) = session.display
        {
            entry.app.name = display;
            entry.named = true;
        }
        return;
    }
    let binary = file_name(&session.path);
    let named = session.display.is_some();
    let name = session.display.unwrap_or_else(|| stem(&binary).to_string());
    entries.push(Entry {
        app: AudioApp {
            id: session.path,
            name,
            bundle_id: None,
            binary: Some(binary),
            pid: session.pid,
            playing: session.playing,
        },
        named,
    });
}

/// Playing apps first, then by name, path and process.
fn finish(entries: Vec<Entry>) -> Vec<AudioApp> {
    let mut apps: Vec<AudioApp> = entries.into_iter().map(|entry| entry.app).collect();
    apps.sort_by_cached_key(|app| {
        (
            !app.playing,
            app.name.to_lowercase(),
            app.id.to_lowercase(),
            app.pid,
        )
    });
    apps
}

/// The stable id `selector` names among `apps`: an exact id (ignoring
/// case, as Windows paths do), else an exact name or binary (with or
/// without `.exe`), else part of one name. The first rule that matches
/// decides; processes sharing an id are one app. An absolute executable path
/// may be chosen before launch; an unknown name is an actionable error.
pub(crate) fn resolve(selector: &str, apps: &[AudioApp]) -> Result<String> {
    ensure!(
        !selector.trim().is_empty() && !selector.contains('\0') && selector.len() <= MAX_TEXT,
        "application_audio.app: expected a nonempty executable path, app name or binary \
         without NUL"
    );
    let query = selector.to_lowercase();
    let rules: [&dyn Fn(&AudioApp) -> bool; 3] = [
        &|app: &AudioApp| app.id.to_lowercase() == query,
        &|app: &AudioApp| {
            app.name.to_lowercase() == query
                || app.binary.as_deref().is_some_and(|binary| {
                    let binary = binary.to_lowercase();
                    binary == query || stem(&binary) == query
                })
        },
        &|app: &AudioApp| app.name.to_lowercase().contains(&query),
    ];
    for rule in rules {
        let ids: BTreeMap<String, &str> = apps
            .iter()
            .filter(|app| rule(app))
            .map(|app| (app.id.to_lowercase(), app.id.as_str()))
            .collect();
        match ids.len() {
            0 => continue,
            1 => return Ok(ids.into_values().next().unwrap_or(selector).to_string()),
            found => bail!(
                "application_audio.app: {selector:?} is ambiguous ({found} apps); use one of \
                 these ids: {}",
                choices(ids.into_values().map(|id| format!("{id:?}")))
            ),
        }
    }
    if Path::new(selector).is_absolute() {
        return Ok(selector.replace('/', "\\"));
    }
    bail!(
        "application_audio.app: no listed app matches {selector:?}; to select an app before launch, use its full executable path. Listed ids: {}",
        choices(apps.iter().map(|app| format!("{:?}", app.id)))
    )
}

fn choices(items: impl Iterator<Item = String>) -> String {
    let mut items: Vec<String> = items.collect();
    let extra = items.len().saturating_sub(MAX_CHOICES);
    items.truncate(MAX_CHOICES);
    let mut text = items.join(", ");
    if extra > 0 {
        text.push_str(&format!(" and {extra} more"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(path: &str, name: &str, pid: u32) -> AudioApp {
        AudioApp {
            id: path.into(),
            name: name.into(),
            bundle_id: None,
            binary: Some(file_name(path)),
            pid,
            playing: false,
        }
    }

    fn session(pid: u32, path: &str, display: Option<&str>, playing: bool) -> Session {
        Session {
            pid,
            path: path.into(),
            display: display.map(str::to_string),
            playing,
        }
    }

    const SPOTIFY: &str = r"C:\Users\me\AppData\Roaming\Spotify\Spotify.exe";
    const CHROME: &str = r"C:\Program Files\Google\Chrome\Application\chrome.exe";
    const CODE: &str = r"C:\Program Files\Microsoft VS Code\Code.exe";
    const CODE_HELPER: &str = r"C:\Tools\CodeHelper.exe";

    fn listed() -> Vec<AudioApp> {
        vec![
            app(SPOTIFY, "Spotify", 10),
            app(CHROME, "Google Chrome", 20),
            app(CHROME, "Google Chrome", 21),
            app(CODE, "Code", 30),
            app(CODE_HELPER, "Code Helper", 40),
        ]
    }

    #[test]
    fn exact_id_matches_ignoring_case() {
        let apps = listed();
        assert_eq!(resolve(SPOTIFY, &apps).unwrap(), SPOTIFY);
        assert_eq!(resolve(&CODE.to_uppercase(), &apps).unwrap(), CODE);
    }

    #[test]
    fn name_and_binary_match_exactly() {
        let apps = listed();
        assert_eq!(resolve("spotify", &apps).unwrap(), SPOTIFY);
        assert_eq!(resolve("CHROME.EXE", &apps).unwrap(), CHROME);
        assert_eq!(resolve("chrome", &apps).unwrap(), CHROME);
        assert_eq!(resolve("Google Chrome", &apps).unwrap(), CHROME);
    }

    #[test]
    fn exact_matches_win_over_parts_of_names() {
        let apps = listed();
        // "code" is part of both names but the exact name of one.
        assert_eq!(resolve("code", &apps).unwrap(), CODE);
        assert_eq!(resolve("helper", &apps).unwrap(), CODE_HELPER);
        assert_eq!(resolve("goog", &apps).unwrap(), CHROME);
        // An exact id wins even when it is part of no name.
        let mut apps = apps;
        apps.push(app(r"C:\x\Code.exe", "Other", 50));
        assert_eq!(resolve(r"c:\x\code.exe", &apps).unwrap(), r"C:\x\Code.exe");
    }

    #[test]
    fn several_processes_of_one_app_are_not_ambiguous() {
        let apps = listed();
        assert_eq!(resolve("Chrome", &apps).unwrap(), CHROME);
        assert_eq!(resolve("chrom", &apps).unwrap(), CHROME);
    }

    #[test]
    fn ambiguity_lists_the_candidate_ids() {
        let mut apps = listed();
        apps.push(app(r"D:\Portable\Spotify.exe", "Spotify", 60));
        let error = resolve("Spotify", &apps).unwrap_err().to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("Spotify.exe"), "{error}");
        assert!(error.contains(r"D:\\Portable"), "{error}");
        // Part of two names: ambiguous, not the first listed.
        let error = resolve("o", &apps).unwrap_err().to_string();
        assert!(error.contains("ambiguous"), "{error}");
        // The exact id still picks one.
        assert_eq!(resolve(SPOTIFY, &apps).unwrap(), SPOTIFY);
    }

    #[test]
    fn apps_not_running_need_a_full_executable_path() {
        let apps = listed();
        let error = resolve("Discord", &apps).unwrap_err().to_string();
        assert!(error.contains("full executable path"), "{error}");
        assert!(error.contains("Spotify.exe"), "{error}");
        assert_eq!(
            resolve(r"C:\Apps\obs64.exe", &[]).unwrap(),
            r"C:\Apps\obs64.exe"
        );
        assert_eq!(
            resolve("C:/Apps/obs64.exe", &[]).unwrap(),
            r"C:\Apps\obs64.exe"
        );
    }

    #[test]
    fn invalid_selectors_are_refused() {
        let apps = listed();
        for selector in ["", "  ", "Spotify\0x"] {
            let error = resolve(selector, &apps).unwrap_err().to_string();
            assert!(error.contains("application_audio.app"), "{error}");
        }
        assert!(resolve(&"a".repeat(MAX_TEXT + 1), &apps).is_err());
    }

    #[test]
    fn sessions_merge_per_process_and_sort_playing_first() {
        let mut entries = Vec::new();
        merge(&mut entries, session(20, CHROME, None, false));
        merge(
            &mut entries,
            session(20, CHROME, Some("Google Chrome"), true),
        );
        merge(&mut entries, session(21, CHROME, None, false));
        merge(&mut entries, session(10, SPOTIFY, Some("Spotify"), false));
        let apps = finish(entries);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].pid, 20);
        assert!(apps[0].playing);
        assert_eq!(apps[0].name, "Google Chrome");
        assert_eq!(apps[0].binary.as_deref(), Some("chrome.exe"));
        assert_eq!(apps[0].id, CHROME);
        assert_eq!((apps[1].name.as_str(), apps[1].pid), ("chrome", 21));
        assert_eq!((apps[2].name.as_str(), apps[2].pid), ("Spotify", 10));
        assert!(apps.iter().all(|app| app.bundle_id.is_none()));
    }

    #[test]
    fn unusable_display_names_fall_back_to_the_executable() {
        assert_eq!(display_name("  Spotify "), Some("Spotify".into()));
        assert_eq!(display_name(""), None);
        assert_eq!(display_name("   "), None);
        assert_eq!(
            display_name(r"@%SystemRoot%\System32\AudioSrv.Dll,-202"),
            None
        );
        assert_eq!(display_name("a\u{7}b"), None);
        assert_eq!(stem("Code.EXE"), "Code");
        assert_eq!(stem(".exe"), ".exe");
        assert_eq!(stem("tool"), "tool");
    }

    /// Hosted runners have no audio devices: listing must still succeed,
    /// and whatever is listed must be addressable by its own id.
    #[test]
    fn native_listing_succeeds_and_is_addressable() {
        let apps = audio_apps().expect("audio apps");
        let own = std::process::id();
        for listed in &apps {
            assert!(!listed.id.is_empty() && listed.pid != 0 && listed.pid != own);
            assert_eq!(
                listed.binary.as_deref(),
                Some(file_name(&listed.id).as_str())
            );
            assert!(!listed.name.is_empty());
            assert_eq!(resolve(&listed.id, &apps).unwrap(), listed.id);
        }
        let mut processes: Vec<_> = apps.iter().map(|app| (app.pid, &app.id)).collect();
        processes.sort();
        processes.dedup();
        assert_eq!(processes.len(), apps.len());
    }
}
