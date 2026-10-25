//! Process-loopback activation and app lifetime following. The packet path is
//! the same bounded WASAPI reader as endpoint capture; never fall back to the
//! desktop mix if a process cannot be opened.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::atomic::AtomicI64;
use std::time::{Duration, Instant};

use anyhow::ensure;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    E_ACCESSDENIED, ERROR_NO_MORE_FILES, FILETIME, HANDLE, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, OSVERSIONINFOW};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Owned, Ref};

use super::*;
use crate::audio_apps;

const MIN_BUILD: u32 = 19041;
const SCAN_INTERVAL: Duration = Duration::from_millis(250);
// Audio handles can close before the owning process object becomes signaled.
// On an API failure, allow that bounded exit tail before failing the source.
const EXIT_GRACE_MS: u32 = 250;

fn require_build(major: u32, build: u32) -> Result<()> {
    if major < 10 || (major == 10 && build < MIN_BUILD) {
        bail!(
            "application_audio.app: process loopback requires Windows 10 version 2004 (build {MIN_BUILD}) or later; this system is {major}, build {build}"
        );
    }
    Ok(())
}

fn supported() -> Result<()> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: a live, sized output structure; unlike GetVersionEx this is not
    // affected by an executable's compatibility manifest.
    unsafe { RtlGetVersion(&mut version) }
        .ok()
        .context("read Windows version")?;
    require_build(version.dwMajorVersion, version.dwBuildNumber)
}

pub fn probe_application_audio(selector: &str) -> Result<String> {
    supported()?;
    let apps = audio_apps::audio_apps()?;
    let identity = audio_apps::resolve(selector, &apps)?;
    ensure!(
        apps.iter()
            .any(|app| app.id.eq_ignore_ascii_case(&identity))
            || Path::new(&identity).is_file(),
        "application_audio.app: no listed app matches {selector:?} and no such executable: {identity:?}"
    );
    Ok(identity)
}

struct Activation {
    params: AUDIOCLIENT_ACTIVATION_PARAMS,
    complete: Event,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    activation: Arc<Activation>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        self.activation.complete.signal();
        Ok(())
    }
}

fn activate(pid: u32, state: &Arc<State>) -> Result<Session> {
    let activation = Arc::new(Activation {
        params: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        },
        complete: Event::new()?,
    });
    // The blob borrows params, not CoTaskMem memory. Do not PropVariantClear
    // it. Windows retains our agile completion object, which owns params,
    // even if cancellation/timeout drops our async-operation reference.
    let variant = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&activation.params as *const AUDIOCLIENT_ACTIVATION_PARAMS)
                            .cast_mut()
                            .cast(),
                    },
                },
                ..Default::default()
            }),
        },
    });
    let completion: IActivateAudioInterfaceCompletionHandler = Completion {
        activation: activation.clone(),
    }
    .into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*variant), &completion)
    }.context("application_audio.app: activate Windows process loopback (requires Windows 10 build 19041 or later)")?;
    let wait = unsafe {
        WaitForMultipleObjects(
            &[state.stop.handle(), activation.complete.handle()],
            false,
            5000,
        )
    };
    if wait == WAIT_OBJECT_0 {
        bail!("application audio activation cancelled");
    }
    if wait.0 != WAIT_OBJECT_0.0 + 1 {
        bail!(
            "application_audio.app: process-loopback activation did not complete within 5 seconds (wait={})",
            wait.0
        );
    }
    let mut status = HRESULT(0);
    let mut unknown = None;
    unsafe { operation.GetActivateResult(&mut status, &mut unknown) }
        .context("process-loopback activation result")?;
    status
        .ok()
        .context("application_audio.app: Windows refused process-loopback activation")?;
    let client: IAudioClient = unknown
        .context("process-loopback returned no audio client")?
        .cast()?;
    // Process loopback has no endpoint mix format or session-control service.
    // Request the engine's stereo float format directly; Windows performs the
    // source-format conversion before handing packets to our shared reader.
    let format = capture_format(2, 3);
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            &format.Format,
            None,
        )
    }
    .context("process-loopback Initialize: stereo 48000 Hz float32")?;
    // Windows 10's virtual client returns S_OK/zero from GetBufferSize. It
    // has no endpoint buffer to query. Apply our existing hard packet bound
    // instead; GetNextPacketSize/GetBuffer are still validated before reading.
    let mut session =
        Session::with_buffer(client, 2, vec![0, 1], SAMPLE_RATE, false, MAX_FRAMES, state)?;
    session.process_loopback = true;
    Ok(session)
}

fn image_name(process: HANDLE) -> Option<String> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&path[..length as usize]))
}

/// Pin the actual process object, not merely its reusable PID. Recheck its
/// executable after opening it so a stale session cannot select a new owner.
/// The error is OpenProcess's own, so callers can tell a denial (elevation,
/// another account) from a process that is already gone.
fn open_process(pid: u32, identity: &str) -> windows::core::Result<Option<Owned<HANDLE>>> {
    let process = unsafe {
        Owned::new(OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?)
    };
    if unsafe { WaitForSingleObject(*process, 0) } != WAIT_TIMEOUT {
        return Ok(None);
    }
    // A process can exit after the snapshot/open and before this query.
    // An unreadable identity is never a license to capture its PID.
    let Some(actual) = image_name(*process) else {
        return Ok(None);
    };
    Ok((actual.eq_ignore_ascii_case(identity)
        && unsafe { WaitForSingleObject(*process, 0) } == WAIT_TIMEOUT)
        .then_some(process))
}

fn filetime(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// The creation time of whatever process holds `pid` now. An exited process
/// that is still referenced keeps its PID, so its time remains its own.
fn creation_time(pid: u32) -> Option<u64> {
    let process =
        unsafe { Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?) };
    let mut created = FILETIME::default();
    let (mut exited, mut kernel, mut user) = (created, created, created);
    unsafe { GetProcessTimes(*process, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some(filetime(created))
}

/// Keep the matches that have no matching ancestor. Toolhelp reports a
/// parent's PID even after that parent exits, and Windows may give the PID to
/// an unrelated process, so every traversed edge must prove itself: both
/// ends' creation times are readable, both processes existed before the
/// snapshot (`taken`), and the parent is strictly older than its child.
/// Anything unproven stops the walk and leaves the match a root, which fails
/// closed into the multiple-tree refusal rather than merging trees.
fn lineage_roots(
    entries: &[(u32, u32)],
    matches: &[u32],
    taken: u64,
    mut created: impl FnMut(u32) -> Option<u64>,
) -> Vec<u32> {
    let mut known = move |pid: u32| created(pid).filter(|&time| time < taken);
    matches
        .iter()
        .copied()
        .filter(|&pid| {
            let mut current = pid;
            let Some(mut current_time) = known(pid) else {
                return true;
            };
            for _ in 0..entries.len() {
                let Some(&(_, parent)) = entries.iter().find(|&&(id, _)| id == current) else {
                    break;
                };
                if parent == 0 || parent == current || parent == pid {
                    break;
                }
                let Some(parent_time) = known(parent).filter(|&time| time < current_time) else {
                    break;
                };
                if matches.contains(&parent) {
                    return false;
                }
                current = parent;
                current_time = parent_time;
            }
            true
        })
        .collect()
}

/// Collapse matching descendants into their highest matching ancestor: a
/// browser's audio-session PID often belongs to a renderer, not its root.
/// Also returns a same-named process that refused to open (access denied).
fn process_roots(identity: &str) -> Result<(Vec<u32>, Option<u32>)> {
    // Read before the snapshot: a process created later cannot be the one the
    // snapshot listed under its PID.
    let taken = filetime(unsafe { GetSystemTimeAsFileTime() });
    let snapshot = unsafe { Owned::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?) };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let binary = identity.rsplit(['\\', '/']).next().unwrap_or(identity);
    let mut entries = Vec::new();
    let mut matches = Vec::new();
    let mut denied = None;
    let mut next = unsafe { Process32FirstW(*snapshot, &mut entry) };
    while next.is_ok() {
        if entries.len() == 4096 {
            bail!("application_audio.app: process snapshot exceeds 4096 entries");
        }
        entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
        let length = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
        if name.eq_ignore_ascii_case(binary) {
            match open_process(entry.th32ProcessID, identity) {
                Ok(Some(_)) => matches.push(entry.th32ProcessID),
                Ok(None) => {}
                Err(error) if error.code() == E_ACCESSDENIED => {
                    denied.get_or_insert(entry.th32ProcessID);
                }
                // Typically the process exited after the snapshot.
                Err(_) => {}
            }
        }
        next = unsafe { Process32NextW(*snapshot, &mut entry) };
    }
    if let Err(error) = next
        && error.code() != HRESULT::from_win32(ERROR_NO_MORE_FILES.0)
    {
        return Err(error).context("application_audio.app: enumerate process tree");
    }
    let mut times = HashMap::new();
    let roots = lineage_roots(&entries, &matches, taken, |pid| {
        *times.entry(pid).or_insert_with(|| creation_time(pid))
    });
    Ok((roots, denied))
}

fn silence_until(sink: &AudioSink, end: &AtomicI64, until: Time) {
    let mut next = end.load(Ordering::Relaxed);
    let until = until.as_nanos();
    // A suspended worker must not allocate or burst an unbounded backlog.
    // Leave long gaps visible to the engine's loss accounting.
    if until.saturating_sub(next) > 1_000_000_000 {
        next = until - 10_000_000;
    }
    let silence = [0.0; 960];
    loop {
        let frames = (i128::from(until.saturating_sub(next)) * i128::from(SAMPLE_RATE)
            / 1_000_000_000)
            .clamp(0, 480) as usize;
        if frames == 0 {
            break;
        }
        (sink.samples)(&silence[..frames * 2], Time::from_nanos(next));
        next += frames as i64 * 1_000_000_000 / i64::from(SAMPLE_RATE);
    }
    end.store(next, Ordering::Relaxed);
}

/// The application-audio worker body, on its own MTA thread. `running`
/// records whether startup was reported through `started`, so the caller
/// knows whether a later error goes to `started` or `sink.stopped`.
fn application_worker(
    identity: String,
    state: &Arc<State>,
    sink: &Arc<AudioSink>,
    started: &mpsc::SyncSender<std::result::Result<(), String>>,
    running: &mut bool,
    thread_start: fn(),
) -> Result<()> {
    let _apartment = Apartment::new()?;
    thread_start();
    let end = Arc::new(AtomicI64::new(crate::now().as_nanos()));
    let mut identity = identity;
    let mut scan = Instant::now();
    let mut warned = false;
    let mut denied_warned = false;
    let mut waiting_warned = false;
    loop {
        if state.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        if Instant::now() >= scan {
            scan = Instant::now() + SCAN_INTERVAL;
            let apps = match audio_apps::current_apps(!*running) {
                Ok(apps) => apps,
                Err(error) if *running => {
                    if !warned {
                        eprintln!("Windows application audio: waiting for app listing: {error:#}");
                        warned = true;
                    }
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            // Keep the executable identity across restarts; display-name
            // changes must never switch the selected application.
            let selection = audio_apps::resolve(&identity, &apps);
            let selected = match selection {
                Ok(selected) => selected,
                Err(error) if *running => {
                    if !warned {
                        eprintln!("Windows application audio: waiting: {error:#}");
                        warned = true;
                    }
                    identity.clone()
                }
                Err(error) => return Err(error),
            };
            let (mut pids, denied) = match process_roots(&selected) {
                Ok(roots) => roots,
                Err(error) if *running => {
                    if !warned {
                        eprintln!(
                            "Windows application audio: waiting for process listing: {error:#}"
                        );
                        warned = true;
                    }
                    (Vec::new(), None)
                }
                Err(error) => return Err(error),
            };
            // Once per source: a denial repeats on every scan. Only access
            // denied is reported; other open failures are usually exits.
            if let Some(pid) = denied
                && !denied_warned
            {
                eprintln!(
                    "Windows application audio: cannot open process {pid}, which shares the executable name of {selected:?}: access denied, so it is skipped; if it is the selected app, it may be running elevated or as another user, and this recorder lacks permission to capture it"
                );
                denied_warned = true;
            }
            if pids.is_empty() && !waiting_warned {
                eprintln!("Windows application audio: waiting for {selected:?} to start");
                waiting_warned = true;
            }
            if pids.len() > 1 {
                if !*running {
                    bail!(
                        "application_audio.app: {selected:?} has multiple independent process trees {pids:?}; close extra instances before capturing"
                    );
                }
                if !warned {
                    eprintln!(
                        "Windows application audio: waiting for one process tree of {selected:?}; candidates {pids:?}"
                    );
                    warned = true;
                }
                pids.clear();
            }
            if let Some(&pid) = pids.first() {
                identity = selected;
                // The root was verified moments ago; any failure here is the
                // same exit/replacement race the next scan resolves.
                if let Ok(Some(process)) = open_process(pid, &identity) {
                    let capture = match activate(pid, state) {
                        Ok(capture) => capture,
                        Err(_)
                            if unsafe { WaitForSingleObject(*process, EXIT_GRACE_MS) }
                                == WAIT_OBJECT_0 =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if let Err(error) = unsafe { capture.client.Start() } {
                        if unsafe { WaitForSingleObject(*process, EXIT_GRACE_MS) } == WAIT_OBJECT_0
                        {
                            continue;
                        }
                        return Err(error).context("process-loopback Start");
                    }
                    warned = false;
                    if !*running {
                        let _ = started.send(Ok(()));
                        *running = true;
                    }
                    let first = Arc::new(AtomicBool::new(true));
                    let forward = AudioSink {
                        samples: Box::new({
                            let sink = sink.clone();
                            let end = end.clone();
                            move |samples, at| {
                                if first.swap(false, Ordering::Relaxed) {
                                    silence_until(&sink, &end, at);
                                }
                                (sink.samples)(samples, at);
                                end.store(
                                    at.as_nanos()
                                        + (samples.len() as i64 / 2) * 1_000_000_000
                                            / i64::from(SAMPLE_RATE),
                                    Ordering::Relaxed,
                                );
                            }
                        }),
                        stopped: Box::new(|_| {}),
                    };
                    // Process exit is a lifecycle event, not source
                    // failure. Drop stops only our loopback client.
                    if let Err(error) = capture.run(state, &forward, Some(*process))
                        && unsafe { WaitForSingleObject(*process, EXIT_GRACE_MS) } != WAIT_OBJECT_0
                    {
                        return Err(error);
                    }
                    scan = Instant::now();
                }
            }
        }
        if !*running {
            let _ = started.send(Ok(()));
            *running = true;
        }
        // Lag idle silence behind wall time; fill the remaining boundary
        // only when the first real packet supplies its actual QPC.
        silence_until(
            sink,
            &end,
            Time::from_nanos(crate::now().as_nanos() - 50_000_000),
        );
        match unsafe { WaitForSingleObject(state.stop.handle(), 10) } {
            WAIT_OBJECT_0 => return Ok(()),
            WAIT_TIMEOUT => {}
            _ => bail!("application audio stop-event wait failed"),
        }
    }
}

pub fn capture_application_audio(
    selector: &str,
    sink: AudioSink,
    thread_start: fn(),
) -> Result<(AudioCapture, u32)> {
    let identity = probe_application_audio(selector)?;
    let state = State::new()?;
    let device = AudioDevice {
        id: identity.clone(),
        name: selector.into(),
        input: false,
        default: false,
        channels: 2,
        sample_rate: SAMPLE_RATE,
    };
    let (started, ready) = mpsc::sync_channel(1);
    let sink = Arc::new(sink);
    let worker = {
        let state = state.clone();
        thread::Builder::new()
            .name("Windows application audio".into())
            .spawn(move || {
                let mut running = false;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    application_worker(
                        identity,
                        &state,
                        &sink,
                        &started,
                        &mut running,
                        thread_start,
                    )
                }));
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("Windows application audio: {error:#}"),
                    Err(_) => "Windows application audio worker panicked".into(),
                };
                if !running {
                    let _ = started.send(Err(error));
                } else if !state.stopping.load(Ordering::Acquire) {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (sink.stopped)(error)
                    }));
                }
            })?
    };
    match ready.recv() {
        Ok(Ok(())) => Ok((
            AudioCapture {
                state,
                worker: Some(worker),
                device,
            },
            2,
        )),
        result => {
            state.stopping.store(true, Ordering::Release);
            state.stop.signal();
            let _ = worker.join();
            match result {
                Ok(Err(error)) => bail!("{error}"),
                _ => bail!("application audio worker exited before startup"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_build_refuses_before_activation() {
        assert!(
            require_build(10, 19040)
                .unwrap_err()
                .to_string()
                .contains("19041")
        );
        assert!(require_build(10, 19041).is_ok());
        assert!(require_build(10, 19045).is_ok());
        assert!(require_build(10, 26100).is_ok());
        assert!(require_build(6, 9600).is_err());
    }

    #[test]
    fn lineage_merges_only_proven_parent_edges() {
        // explorer 1 -> browser 10 -> helper 15 -> renderer 11; 10 and 11 match.
        let entries = [(1, 0), (10, 1), (15, 10), (11, 15)];
        let roots = |times: &[(u32, u64)]| {
            lineage_roots(&entries, &[10, 11], 1000, |pid| {
                times.iter().find(|&&(id, _)| id == pid).map(|&(_, t)| t)
            })
        };
        let tree: [(u32, u64); 4] = [(1, 100), (10, 200), (15, 300), (11, 400)];
        assert_eq!(roots(&tree), [10]);
        // PID 10 now belongs to a browser started after the renderer: the
        // renderer's real parent exited and its PID was reused.
        assert_eq!(
            roots(&[(1, 100), (10, 500), (15, 300), (11, 400)]),
            [10, 11]
        );
        // Same for a reused intermediate, and for a tie.
        assert_eq!(
            roots(&[(1, 100), (10, 200), (15, 450), (11, 400)]),
            [10, 11]
        );
        assert_eq!(
            roots(&[(1, 100), (10, 200), (15, 400), (11, 400)]),
            [10, 11]
        );
        // Unreadable times fail closed: no merge through an unproven edge.
        assert_eq!(roots(&[(1, 100), (10, 200), (11, 400)]), [10, 11]);
        assert_eq!(roots(&[(1, 100), (10, 200), (15, 300)]), [10, 11]);
        // A process created after the snapshot replaced the listed PID.
        assert_eq!(
            roots(&[(1, 100), (10, 200), (15, 300), (11, 1000)]),
            [10, 11]
        );
        assert_eq!(
            roots(&[(1, 100), (10, 1200), (15, 300), (11, 400)]),
            [10, 11]
        );
    }

    #[test]
    fn unlisted_executable_probe_requires_a_file_and_normalizes_slashes() {
        let executable = std::env::current_exe().unwrap();
        let identity = executable.to_str().unwrap();
        assert_eq!(probe_application_audio(identity).unwrap(), identity);
        assert_eq!(
            probe_application_audio(&identity.replace('\\', "/")).unwrap(),
            identity
        );
        let missing = executable.join("no-such-application.exe");
        for path in [missing.as_path(), executable.parent().unwrap()] {
            let error = probe_application_audio(path.to_str().unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains("application_audio.app"), "{error}");
            assert!(error.contains("no such executable"), "{error}");
        }
    }

    #[test]
    fn dormant_application_starts_with_silence_and_stops_without_audio_devices() {
        let folder = std::env::temp_dir().join(format!(
            "isocast-app-wait-{}-{}",
            std::process::id(),
            filetime(unsafe { GetSystemTimeAsFileTime() })
        ));
        std::fs::create_dir(&folder).unwrap();
        let executable = folder.join("dormant.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let (send, receive) = mpsc::channel();
        let sink = AudioSink {
            samples: Box::new(move |samples, _| {
                let _ = send.send((samples.len() / 2, samples.iter().all(|&v| v == 0.0)));
            }),
            stopped: Box::new(|reason| panic!("{reason}")),
        };
        let result = capture_application_audio(
            &executable.to_str().unwrap().replace('\\', "/"),
            sink,
            || {},
        );
        let mut frames = 0;
        let mut silent = true;
        if result.is_ok() {
            let deadline = Instant::now() + Duration::from_secs(3);
            // Observe several idle scans without sleeping or starting a player.
            while frames < SAMPLE_RATE as usize && Instant::now() < deadline {
                let Ok((count, zero)) = receive.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                frames += count;
                silent &= zero;
            }
        }
        let started = result.is_ok();
        drop(result);
        std::fs::remove_file(&executable).unwrap();
        std::fs::remove_dir(&folder).unwrap();
        assert!(started);
        assert!(silent && frames >= SAMPLE_RATE as usize);
    }

    #[test]
    fn own_process_is_one_root_and_open_errors_keep_their_cause() {
        use windows::Win32::System::Threading::GetCurrentProcess;
        let pid = std::process::id();
        let identity = image_name(unsafe { GetCurrentProcess() }).unwrap();
        assert!(open_process(pid, &identity).unwrap().is_some());
        let selected = audio_apps::resolve(&identity.replace('\\', "/"), &[]).unwrap();
        assert!(open_process(pid, &selected).unwrap().is_some());
        assert!(open_process(pid, r"C:\not\isocast.exe").unwrap().is_none());
        // PID 0 is invalid to open, which is not a permission denial.
        let Err(error) = open_process(0, &identity) else {
            panic!("PID 0 opened");
        };
        assert_ne!(error.code(), E_ACCESSDENIED);
        assert_eq!(process_roots(&identity).unwrap(), (vec![pid], None));
        assert!(creation_time(pid).unwrap() < filetime(unsafe { GetSystemTimeAsFileTime() }));
    }

    #[test]
    fn process_format_is_engine_stereo_float() {
        let format = capture_format(2, 3);
        let rate = format.Format.nSamplesPerSec;
        let channels = format.Format.nChannels;
        let bits = format.Format.wBitsPerSample;
        let align = format.Format.nBlockAlign;
        assert_eq!((rate, channels, bits, align), (48000, 2, 32, 8));
        let subformat = format.SubFormat;
        assert_eq!(subformat, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }

    #[test]
    fn virtual_device_uses_qpc_and_surfaces_gaps_without_device_positions() {
        let mut clock = PacketClock::new(SAMPLE_RATE, MAX_FRAMES, false);
        for packet in 0..10 {
            let qpc = 1_000_000 + packet * 100_000;
            let (time, gap) = clock.process_timestamp(480, qpc, 0).unwrap();
            assert_eq!(time.as_nanos(), qpc as i64 * 100);
            assert_eq!(gap, 0);
        }
        assert_eq!(clock.process_timestamp(480, 2_100_000, 0).unwrap().1, 480);
        assert!(clock.process_timestamp(480, 2_100_000, 0).is_err());
        assert!(
            clock
                .process_timestamp(
                    480,
                    2_200_000,
                    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32
                )
                .is_err()
        );
    }

    #[test]
    fn idle_silence_ends_on_the_real_packet_boundary_without_a_short_gap() {
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = AudioSink {
            samples: Box::new({
                let delivered = delivered.clone();
                move |samples, at| {
                    assert!(samples.iter().all(|&v| v == 0.0));
                    delivered
                        .lock()
                        .unwrap()
                        .push((samples.len() / 2, at.as_nanos()));
                }
            }),
            stopped: Box::new(|reason| panic!("{reason}")),
        };
        let end = AtomicI64::new(1_000_000_000);
        silence_until(&sink, &end, Time::from_nanos(1_015_000_000));
        assert_eq!(
            *delivered.lock().unwrap(),
            [(480, 1_000_000_000), (240, 1_010_000_000)]
        );
        assert_eq!(end.load(Ordering::Relaxed), 1_015_000_000);
        silence_until(&sink, &end, Time::from_nanos(1_014_000_000));
        assert_eq!(delivered.lock().unwrap().len(), 2);
    }
}
