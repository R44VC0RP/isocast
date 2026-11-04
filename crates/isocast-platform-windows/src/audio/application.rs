//! Process-loopback activation and app lifetime following. The packet path is
//! the same bounded WASAPI reader as endpoint capture; never fall back to the
//! desktop mix if a process cannot be opened.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::ManuallyDrop;
use std::path::Path;
use std::process::ChildStdin;
use std::sync::atomic::AtomicI64;
use std::time::{Duration, Instant};

use anyhow::ensure;
use parking_lot::Mutex;
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    E_ACCESSDENIED, ERROR_NO_MORE_FILES, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
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
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, SetEvent, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Owned, Ref};

use super::*;
use crate::audio_apps;

mod exclusion;
pub(in crate::audio) use exclusion::capture_desktop;
pub(crate) use exclusion::probe_system_audio;

const MIN_BUILD: u32 = 19041;
const SCAN_INTERVAL: Duration = Duration::from_millis(250);
// Audio handles can close before the owning process object becomes signaled.
// On an API failure, allow that bounded exit tail before failing the source.
const EXIT_GRACE_MS: u32 = 250;

fn require_build(major: u32, build: u32) -> Result<()> {
    if major < 10 || (major == 10 && build < MIN_BUILD) {
        bail!(
            "process loopback requires Windows 10 version 2004 (build {MIN_BUILD}) or later; this system is {major}, build {build}"
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
    supported().map_err(|e| anyhow!("application_audio.app: {e:#}"))?;
    let apps = audio_apps::audio_apps()?;
    let identity = audio_apps::resolve(selector, &apps)?;
    ensure!(
        apps.iter()
            .any(|app| app.id.eq_ignore_ascii_case(&identity))
            || Path::new(&identity).is_file(),
        "application_audio.app: no listed app matches {selector:?} and no such executable: {identity:?}"
    );
    if let Ok((roots, _)) = process_roots(&identity)
        && let [root] = roots.as_slice()
    {
        audio_apps::check_single_endpoint(&identity, &descendant_pids(*root))?;
    }
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

pub(super) fn activate_mode(
    pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
    state: &Arc<State>,
) -> Result<Session> {
    let activation = Arc::new(Activation {
        params: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: mode,
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
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*variant),
            &completion,
        )
    }
    .context(
        "activate Windows process loopback (requires Windows with process-loopback support)",
    )?;
    let wait = unsafe {
        WaitForMultipleObjects(
            &[state.stop.handle(), activation.complete.handle()],
            false,
            5000,
        )
    };
    if wait == WAIT_OBJECT_0 {
        bail!("process-loopback activation cancelled");
    }
    if wait.0 != WAIT_OBJECT_0.0 + 1 {
        bail!(
            "process-loopback activation did not complete within 5 seconds (wait={})",
            wait.0
        );
    }
    let mut status = HRESULT(0);
    let mut unknown = None;
    unsafe { operation.GetActivateResult(&mut status, &mut unknown) }
        .context("process-loopback activation result")?;
    status
        .ok()
        .context("Windows refused process-loopback activation")?;
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
    let event = Event::new()?;
    let mut session =
        Session::with_buffer(client, event, 2, vec![0, 1], SAMPLE_RATE, false, MAX_FRAMES)?;
    session.process_loopback = true;
    Ok(session)
}

const HELPER_RING_SLOTS: usize = 64;
const HELPER_MAGIC: [u8; 4] = *b"ISAP";
const HELPER_VERSION: u8 = 1;
const HELPER_STARTUP_MS: u32 = 5000;

struct HelperSlot {
    frames: u32,
    flags: u32,
    qpc: u64,
    samples: Vec<f32>,
}

struct HelperQueue {
    slots: Vec<HelperSlot>,
    head: usize,
    len: usize,
    eof: bool,
    error: Option<String>,
}

impl HelperQueue {
    fn new() -> Self {
        let slots = (0..HELPER_RING_SLOTS)
            .map(|_| HelperSlot {
                frames: 0,
                flags: 0,
                qpc: 0,
                samples: Vec::with_capacity(960),
            })
            .collect();
        Self {
            slots,
            head: 0,
            len: 0,
            eof: false,
            error: None,
        }
    }

    fn pop(&mut self, out_samples: &mut Vec<f32>) -> Result<Option<(u32, u32, u64)>> {
        if self.len > 0 {
            let head = self.head;
            let slot = &mut self.slots[head];
            let (frames, flags, qpc) = (slot.frames, slot.flags, slot.qpc);
            std::mem::swap(&mut slot.samples, out_samples);
            self.head = (head + 1) % HELPER_RING_SLOTS;
            self.len -= 1;
            return Ok(Some((frames, flags, qpc)));
        }
        if let Some(err) = &self.error {
            bail!("{err}");
        }
        if self.eof {
            bail!("process-loopback helper stream ended");
        }
        Ok(None)
    }
}

fn write_helper_startup_ok(out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(&HELPER_MAGIC)?;
    out.write_all(&[HELPER_VERSION, 0])?;
    out.flush()
}

fn write_helper_startup_err(out: &mut impl Write, message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(4096) as u32;
    let _ = out.write_all(&HELPER_MAGIC);
    let _ = out.write_all(&[HELPER_VERSION, 1]);
    let _ = out.write_all(&len.to_le_bytes());
    let _ = out.write_all(&bytes[..len as usize]);
    let _ = out.flush();
}

fn read_helper_startup(reader: &mut impl Read) -> Result<()> {
    let mut hdr = [0u8; 6];
    reader
        .read_exact(&mut hdr)
        .context("process-loopback helper exited before startup handshake")?;
    ensure!(
        hdr[..4] == HELPER_MAGIC && hdr[4] == HELPER_VERSION,
        "process-loopback helper sent invalid startup header {:02x?}",
        hdr
    );
    match hdr[5] {
        0 => Ok(()),
        1 => {
            let mut len_buf = [0u8; 4];
            let msg = if reader.read_exact(&mut len_buf).is_ok() {
                let n = (u32::from_le_bytes(len_buf) as usize).min(4096);
                let mut bytes = vec![0u8; n];
                if reader.read_exact(&mut bytes).is_ok() {
                    String::from_utf8_lossy(&bytes).into_owned()
                } else {
                    "process-loopback helper truncated startup error".into()
                }
            } else {
                "unknown helper startup error".into()
            };
            bail!("{msg}");
        }
        status => bail!("process-loopback helper sent unknown startup status {status}"),
    }
}

pub(super) struct HelperClient {
    stdin: Option<ChildStdin>,
    process: crate::broker::Process,
    queue: Arc<Mutex<HelperQueue>>,
    reader: Option<JoinHandle<()>>,
}

impl HelperClient {
    fn spawn(
        pid: u32,
        mode: PROCESS_LOOPBACK_MODE,
        wake_handle: HANDLE,
        state: &State,
    ) -> Result<Self> {
        let helper = std::env::current_exe()?.with_file_name("isocast-capture.exe");
        let (pid_arg, mode_arg) = (pid.to_string(), mode.0.to_string());
        let (process, stdin, stdout) = crate::broker::Process::spawn_with_args(
            &helper,
            &["--process-loopback", &pid_arg, &mode_arg],
        )?;
        let queue = Arc::new(Mutex::new(HelperQueue::new()));
        let ready_event = Arc::new(Event::new()?);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let mut client = Self {
            stdin: Some(stdin),
            process,
            queue: queue.clone(),
            reader: None,
        };
        let wake = wake_handle.0 as usize;
        let reader_ready = ready_event.clone();
        let thread = thread::Builder::new()
            .name("Windows app-audio pipe".into())
            .spawn(move || {
                let mut reader = BufReader::with_capacity(65536, stdout);
                let startup = read_helper_startup(&mut reader);
                let ok = startup.is_ok();
                let _ = ready_tx.send(startup);
                reader_ready.signal();
                if ok {
                    helper_pipe_reader(reader, queue, wake);
                }
            })?;
        client.reader = Some(thread);
        let wait = unsafe {
            WaitForMultipleObjects(
                &[state.stop.handle(), ready_event.handle()],
                false,
                HELPER_STARTUP_MS,
            )
        };
        if wait == WAIT_OBJECT_0 {
            client.process.shutdown(false);
            bail!("process-loopback helper startup cancelled");
        }
        if wait.0 != WAIT_OBJECT_0.0 + 1 {
            client.process.shutdown(false);
            bail!(
                "process-loopback helper did not complete startup within 5 seconds (wait={})",
                wait.0
            );
        }
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(client),
            Ok(Err(err)) => {
                client.process.shutdown(false);
                Err(err)
            }
            Err(_) => {
                client.process.shutdown(false);
                bail!("process-loopback helper startup channel closed");
            }
        }
    }

    pub(super) fn next_packet(
        &self,
        out_samples: &mut Vec<f32>,
    ) -> Result<Option<(u32, u32, u64)>> {
        self.queue.lock().pop(out_samples)
    }
}

impl Drop for HelperClient {
    fn drop(&mut self) {
        // Closing stdin signals the helper's EOF watcher to stop cleanly;
        // Process::shutdown bounds graceful teardown to 250 ms before killing
        // the job and process so reader.join() cannot hang.
        drop(self.stdin.take());
        self.process.shutdown(true);
        if let Some(reader) = self.reader.take()
            && reader.thread().id() != thread::current().id()
        {
            let _ = reader.join();
        }
    }
}

fn helper_pipe_reader(
    mut reader: BufReader<std::process::ChildStdout>,
    queue: Arc<Mutex<HelperQueue>>,
    wake: usize,
) {
    let mut scratch = Vec::with_capacity(960);
    let mut byte_buf = vec![0u8; 960 * 4];
    loop {
        let mut tag = [0u8; 1];
        if reader.read_exact(&mut tag).is_err() {
            let mut q = queue.lock();
            q.error
                .get_or_insert_with(|| "process-loopback helper exited unexpectedly".into());
            q.eof = true;
            drop(q);
            let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
            return;
        }
        match tag[0] {
            1 => {
                let mut hdr = [0u8; 20];
                if reader.read_exact(&mut hdr).is_err() {
                    let mut q = queue.lock();
                    q.error
                        .get_or_insert_with(|| "process-loopback helper pipe truncated".into());
                    drop(q);
                    let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                    return;
                }
                let frames = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
                let flags = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
                let qpc = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
                let payload = u32::from_le_bytes(hdr[16..20].try_into().unwrap()) as usize;
                if frames == 0
                    || frames > MAX_FRAMES
                    || (payload != 0 && payload != frames as usize * 8)
                {
                    let mut q = queue.lock();
                    q.error.get_or_insert_with(|| {
                        "process-loopback helper sent invalid packet header".into()
                    });
                    drop(q);
                    let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                    return;
                }
                scratch.clear();
                if payload == 0 {
                    scratch.resize(frames as usize * 2, 0.0);
                } else {
                    if byte_buf.len() < payload {
                        byte_buf.resize(payload, 0);
                    }
                    if reader.read_exact(&mut byte_buf[..payload]).is_err() {
                        let mut q = queue.lock();
                        q.error.get_or_insert_with(|| {
                            "process-loopback helper payload truncated".into()
                        });
                        drop(q);
                        let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                        return;
                    }
                    for chunk in byte_buf[..payload].as_chunks::<4>().0 {
                        scratch.push(f32::from_le_bytes(*chunk));
                    }
                }
                {
                    let mut q = queue.lock();
                    if q.len == HELPER_RING_SLOTS {
                        q.error.get_or_insert_with(|| {
                            "process-loopback helper ring overflowed (64 packets)".into()
                        });
                        drop(q);
                        let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                        return;
                    }
                    let idx = (q.head + q.len) % HELPER_RING_SLOTS;
                    let slot = &mut q.slots[idx];
                    slot.frames = frames;
                    slot.flags = flags;
                    slot.qpc = qpc;
                    std::mem::swap(&mut slot.samples, &mut scratch);
                    q.len += 1;
                }
                let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
            }
            2 => {
                let mut q = queue.lock();
                q.eof = true;
                drop(q);
                let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                return;
            }
            3 => {
                let mut len_buf = [0u8; 4];
                let msg = if reader.read_exact(&mut len_buf).is_ok() {
                    let n = (u32::from_le_bytes(len_buf) as usize).min(4096);
                    let mut bytes = vec![0u8; n];
                    let _ = reader.read_exact(&mut bytes);
                    String::from_utf8_lossy(&bytes).into_owned()
                } else {
                    "process-loopback helper failed".into()
                };
                let mut q = queue.lock();
                q.error.get_or_insert(msg);
                drop(q);
                let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                return;
            }
            _ => {
                let mut q = queue.lock();
                q.error
                    .get_or_insert_with(|| "process-loopback helper protocol error".into());
                drop(q);
                let _ = unsafe { SetEvent(HANDLE(wake as *mut _)) };
                return;
            }
        }
    }
}

/// Entry point of `isocast-capture.exe --process-loopback <pid> <mode>`.
/// Runs one `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` client in its own OS
/// process: Windows `AudioSes.dll` rejects a second initialized process-loopback
/// client in the same process (`0x8000FFFF`), which would otherwise block
/// zero-gap pre-wrap session splices.
pub fn process_audio_helper_main() -> ! {
    let mut out = BufWriter::with_capacity(65536, std::io::stdout().lock());
    let fail_startup = |out: &mut BufWriter<std::io::StdoutLock<'_>>, err: anyhow::Error| -> ! {
        write_helper_startup_err(out, &format!("{err:#}"));
        std::process::exit(1);
    };
    let state = match State::new() {
        Ok(s) => s,
        Err(e) => fail_startup(&mut out, e),
    };
    let stop_eof = state.clone();
    let _ = thread::Builder::new()
        .name("process-audio-stdin".into())
        .spawn(move || {
            let mut b = [0u8; 1];
            let _ = std::io::stdin().read(&mut b);
            stop_eof.stopping.store(true, Ordering::Release);
            stop_eof.stop.signal();
        });
    let args: Vec<String> = std::env::args().collect();
    let (Ok(pid), Ok(mode_raw)) = (
        args.get(2).unwrap_or(&String::new()).parse::<u32>(),
        args.get(3).unwrap_or(&String::new()).parse::<i32>(),
    ) else {
        fail_startup(&mut out, anyhow!("invalid --process-loopback arguments"));
    };
    let _apt = match Apartment::new() {
        Ok(a) => a,
        Err(e) => fail_startup(&mut out, e),
    };
    let _ = crate::audio_thread();
    let session = match activate_mode(pid, PROCESS_LOOPBACK_MODE(mode_raw), &state) {
        Ok(s) => s,
        Err(e) => fail_startup(&mut out, e),
    };
    if let Err(e) = session.start_client() {
        fail_startup(&mut out, e);
    }
    if write_helper_startup_ok(&mut out).is_err() {
        std::process::exit(0);
    }
    let SessionClient::Wasapi { capture, .. } = &session.client else {
        unreachable!()
    };
    let target_proc = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
        .ok()
        .map(|h| unsafe { Owned::new(h) });
    let mut handles = [
        state.stop.handle(),
        session.event.handle(),
        HANDLE::default(),
    ];
    let count = if let Some(p) = &target_proc {
        handles[2] = **p;
        3
    } else {
        2
    };
    loop {
        let wait = unsafe { WaitForMultipleObjects(&handles[..count], false, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            let _ = out.write_all(&[2u8]);
            let _ = out.flush();
            drop(session);
            std::process::exit(0);
        }
        if wait == WAIT_FAILED {
            drop(session);
            std::process::exit(1);
        }
        let exited = count == 3 && wait.0 == WAIT_OBJECT_0.0 + 2;
        let step: Result<()> = (|| {
            loop {
                let next = unsafe { capture.GetNextPacketSize() }.context("GetNextPacketSize")?;
                if next == 0 {
                    break;
                }
                if next > MAX_FRAMES {
                    bail!("WASAPI packet {next} exceeds {MAX_FRAMES}");
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags, mut pos, mut qpc) = (0, 0, 0, 0);
                unsafe {
                    capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        Some(&mut pos),
                        Some(&mut qpc),
                    )
                }
                .context("GetBuffer")?;
                let lease = Packet {
                    capture,
                    frames,
                    held: true,
                };
                if frames == 0 || frames > MAX_FRAMES {
                    bail!("invalid packet frames {frames}");
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let payload_bytes = if silent { 0u32 } else { frames * 8 };
                out.write_all(&[1u8])?;
                out.write_all(&frames.to_le_bytes())?;
                out.write_all(&flags.to_le_bytes())?;
                out.write_all(&qpc.to_le_bytes())?;
                out.write_all(&payload_bytes.to_le_bytes())?;
                if !silent {
                    let len = frames as usize * 2;
                    let bytes = len * size_of::<f32>();
                    if data.is_null() || !(data as usize).is_multiple_of(align_of::<f32>()) {
                        bail!("invalid float32 packet pointer");
                    }
                    let slice = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), len) };
                    if slice.iter().any(|s| !s.is_finite()) {
                        bail!("WASAPI returned non-finite float32 samples");
                    }
                    let raw = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), bytes) };
                    out.write_all(raw)?;
                }
                lease.release()?;
            }
            out.flush()?;
            Ok(())
        })();
        if let Err(err) = step {
            let msg = format!("{err:#}");
            let bytes = msg.as_bytes();
            let len = bytes.len().min(4096) as u32;
            let _ = out.write_all(&[3u8]);
            let _ = out.write_all(&len.to_le_bytes());
            let _ = out.write_all(&bytes[..len as usize]);
            let _ = out.flush();
            drop(session);
            std::process::exit(1);
        }
        if exited {
            let _ = out.write_all(&[2u8]);
            let _ = out.flush();
            drop(session);
            std::process::exit(0);
        }
    }
}

pub(super) fn open_started_process_loopback(
    pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
    state: &Arc<State>,
) -> Result<Session> {
    if std::env::current_exe()
        .ok()
        .is_some_and(|exe| exe.with_file_name("isocast-capture.exe").is_file())
    {
        let event = Event::new()?;
        let helper = HelperClient::spawn(pid, mode, event.handle(), state)?;
        return Ok(Session::from_helper(helper, event));
    }
    let session = activate_mode(pid, mode, state)?;
    session.start_client()?;
    Ok(session)
}

// Exclude-mode desktop audio (#252) intentionally stays in-process here and
// starts its own client once in exclusion.rs; application audio opens started
// helper sessions through open_started_process_loopback.
fn activate(pid: u32, mode: PROCESS_LOOPBACK_MODE, state: &Arc<State>) -> Result<Session> {
    activate_mode(pid, mode, state)
}

fn image_name(process: HANDLE) -> windows::core::Result<String> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }?;
    Ok(String::from_utf16_lossy(&path[..length as usize]))
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
    let actual = image_name(*process)?;
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
    let (roots, denied) = matching_roots(identity, None)?;
    Ok((roots.into_iter().map(|(pid, _)| pid).collect(), denied))
}

type Root = (u32, Option<Owned<HANDLE>>);

/// Add the recorder to the match set only when a named instance exists.
/// Retain the verified process handles through ancestry checking and, for a
/// named exclusion root, activation. A recycled PID cannot change the filter.
fn matching_roots(identity: &str, recorder: Option<u32>) -> Result<(Vec<Root>, Option<u32>)> {
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
                Ok(Some(handle)) => matches.push((entry.th32ProcessID, Some(handle))),
                Ok(None) => {}
                Err(error) if error.code() == E_ACCESSDENIED => {
                    denied.get_or_insert(entry.th32ProcessID);
                }
                // Unlike inclusion, skipping an unverified candidate can
                // record it. Even an exit race is safer to retry than guess.
                Err(error) if recorder.is_some() => {
                    return Err(error).context("could not verify a candidate executable");
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
    if let Some(pid) = recorder
        && !matches.is_empty()
        && !matches.iter().any(|(id, _)| *id == pid)
    {
        matches.push((pid, None));
    }
    let ids: Vec<_> = matches.iter().map(|(id, _)| *id).collect();
    let mut times = HashMap::new();
    let roots = lineage_roots(&entries, &ids, taken, |pid| {
        *times.entry(pid).or_insert_with(|| creation_time(pid))
    });
    matches.retain(|(pid, _)| roots.contains(pid));
    Ok((matches, denied))
}

/// All live processes in `root_pid`'s creation-time-verified process tree.
pub(super) fn descendant_pids(root_pid: u32) -> Vec<u32> {
    let taken = filetime(unsafe { GetSystemTimeAsFileTime() });
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return vec![root_pid];
    };
    let snapshot = unsafe { Owned::new(snapshot) };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut entries = Vec::new();
    let mut next = unsafe { Process32FirstW(*snapshot, &mut entry) };
    while next.is_ok() && entries.len() < 4096 {
        entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
        next = unsafe { Process32NextW(*snapshot, &mut entry) };
    }
    let mut times = HashMap::new();
    let mut known =
        |pid: u32| (*times.entry(pid).or_insert_with(|| creation_time(pid))).filter(|&t| t < taken);
    let Some(root_time) = known(root_pid) else {
        return vec![root_pid];
    };
    let mut out = vec![root_pid];
    for &(pid, _) in &entries {
        if pid == root_pid {
            continue;
        }
        let mut current = pid;
        let Some(mut current_time) = known(pid) else {
            continue;
        };
        for _ in 0..entries.len() {
            if current == root_pid {
                out.push(pid);
                break;
            }
            let Some(&(_, parent)) = entries.iter().find(|&&(id, _)| id == current) else {
                break;
            };
            if parent == 0 || parent == current || parent == pid {
                break;
            }
            let Some(parent_time) = known(parent).filter(|&t| t < current_time && t >= root_time)
            else {
                break;
            };
            current = parent;
            current_time = parent_time;
        }
    }
    out
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
                    let capture = match open_started_process_loopback(
                        pid,
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                        state,
                    )
                    .context("application_audio.app")
                    {
                        Ok(capture) => capture,
                        Err(_)
                            if unsafe { WaitForSingleObject(*process, EXIT_GRACE_MS) }
                                == WAIT_OBJECT_0 =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let opener: super::process_clock::ProcessSessionOpener = {
                        let state = state.clone();
                        Arc::new(move || {
                            let session = open_started_process_loopback(
                                pid,
                                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                                &state,
                            )
                            .context("application_audio.app")?;
                            Ok(super::process_clock::MtaSession(session))
                        })
                    };
                    let process_clock = match super::process_clock::ProcessClock::start(
                        super::process_clock::ClockTarget::App {
                            root_pid: pid,
                            identity: identity.clone(),
                        },
                        Some(opener),
                        state.clone(),
                        !*running,
                    ) {
                        Ok(clock) => clock,
                        Err(_)
                            if unsafe { WaitForSingleObject(*process, EXIT_GRACE_MS) }
                                == WAIT_OBJECT_0 =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    warned = false;
                    if !*running {
                        let _ = started.send(Ok(()));
                        *running = true;
                    }
                    let first = Arc::new(AtomicBool::new(true));
                    let forward = AudioSink {
                        report_gap: Box::new({
                            let sink = sink.clone();
                            move |until, duration| (sink.report_gap)(until, duration)
                        }),
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
                    if let Err(error) = capture.run_with_clock(
                        state,
                        &forward,
                        Some(*process),
                        Some(process_clock.shared()),
                    ) && unsafe { WaitForSingleObject(*process, EXIT_GRACE_MS) } != WAIT_OBJECT_0
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
    let device = AudioDevice {
        id: identity.clone(),
        name: selector.into(),
        input: false,
        default: false,
        channels: 2,
        sample_rate: SAMPLE_RATE,
    };
    process_capture(device, sink, move |state, sink, started, running| {
        application_worker(identity, state, sink, started, running, thread_start)
    })
}

fn process_capture(
    device: AudioDevice,
    sink: AudioSink,
    run: impl FnOnce(
        &Arc<State>,
        &Arc<AudioSink>,
        &mpsc::SyncSender<std::result::Result<(), String>>,
        &mut bool,
    ) -> Result<()>
    + Send
    + 'static,
) -> Result<(AudioCapture, u32)> {
    let state = State::new()?;
    let (started, ready) = mpsc::sync_channel(1);
    let sink = Arc::new(sink);
    let worker = {
        let state = state.clone();
        thread::Builder::new()
            .name("Windows process loopback".into())
            .spawn(move || {
                let mut running = false;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&state, &sink, &started, &mut running)
                }));
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("Windows process loopback: {error:#}"),
                    Err(_) => "Windows process-loopback worker panicked".into(),
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
                _ => bail!("process-loopback worker exited before startup"),
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
            report_gap: Box::new(|_, gap| panic!("unexpected gap: {gap:?}")),
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
            report_gap: Box::new(|_, gap| panic!("unexpected gap: {gap:?}")),
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

    #[test]
    fn helper_queue_drains_buffered_packets_then_surfaces_eof_and_startup_validates_header() {
        let mut out = Vec::new();
        let mut q = HelperQueue::new();
        assert!(q.pop(&mut out).unwrap().is_none());
        q.slots[0].frames = 480;
        q.slots[0].flags = 0;
        q.slots[0].qpc = 123_456;
        q.slots[0].samples = vec![0.25; 960];
        q.len = 1;
        q.eof = true;
        // Buffered packets before EOF are still delivered first.
        assert_eq!(q.pop(&mut out).unwrap(), Some((480, 0, 123_456)));
        assert_eq!(out.len(), 960);
        // Once empty, EOF returns Err instead of Ok(None) forever.
        assert!(
            q.pop(&mut out)
                .unwrap_err()
                .to_string()
                .contains("helper stream ended")
        );

        let mut ok_wire = Vec::new();
        write_helper_startup_ok(&mut ok_wire).unwrap();
        read_helper_startup(&mut ok_wire.as_slice()).unwrap();

        let mut err_wire = Vec::new();
        write_helper_startup_err(&mut err_wire, "denied");
        assert_eq!(
            read_helper_startup(&mut err_wire.as_slice())
                .unwrap_err()
                .to_string(),
            "denied"
        );
        assert!(read_helper_startup(&mut &[0u8; 6][..]).is_err());
        assert!(read_helper_startup(&mut &[b'I', b'S', b'A', b'P', 99, 0][..]).is_err());
    }
}
