//! Browser sources: each page renders in its own `isocast-browser.exe`
//! helper process (a WebView2 composition controller whose visual is
//! captured offscreen; see crates/isocast-browser), which streams pictures
//! and sound back over a pipe, in the records of
//! crates/isocast-browser-protocol (the Linux helper's). WebView2 never runs
//! inside isocastd: a missing runtime fails only the source, with where to
//! get it; a crash, a hang or a page that grows past the memory cap
//! restarts the helper (with backoff) while the engine and its other
//! sources keep running.
//!
//! The helper and WebView2's browser processes it starts share one
//! kill-on-close job, so they all end with the source or with isocastd, and
//! their memory is counted together.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_browser_protocol as protocol;
use isocast_engine::{
    AudioSink, BrowserControl, BrowserPage, CaptureFormat, FramePool, PageLocation, SAMPLE_RATE,
    Time, VideoSink,
};
use windows::Win32::Foundation::SetHandleInformation;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::core::Owned;

/// How long a new helper may take to say it is loading the page (WebView2
/// starts its own browser first).
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// No picture or beat for this long from a loaded page: the helper
/// hung.
#[cfg(not(test))]
const HANG_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const HANG_TIMEOUT: Duration = Duration::from_millis(800);
/// Restart delays after a crash or hang, doubling up to the last.
#[cfg(not(test))]
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(30),
];
#[cfg(test)]
const BACKOFF: [Duration; 1] = [Duration::from_millis(100)];
/// A helper that ran this long before failing starts the backoff afresh.
const STABLE: Duration = Duration::from_secs(60);
/// The helper and its WebView2 processes together (working sets); more,
/// and the page is restarted. `ISOCAST_BROWSER_MEMORY_MB` overrides it.
const MEMORY_CAP_MB: u64 = 1536;
/// How far the page's audio may drift from the host clock before its
/// timestamps are reset to it (as on macOS and Linux).
const AUDIO_RESYNC: Duration = Duration::from_millis(150);

/// When the first of `frames` frames that have just arrived was made.
#[derive(Default)]
struct Stamper {
    next: Option<Time>,
}

impl Stamper {
    fn stamp(&mut self, frames: usize, now: Time) -> Time {
        let made =
            now - Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(SAMPLE_RATE));
        let limit = AUDIO_RESYNC.as_nanos() as i64;
        let at = match self.next {
            Some(next) if (next.as_nanos() - made.as_nanos()).abs() < limit => next,
            _ => made,
        };
        self.next = Some(at.add_ticks(frames as u64, SAMPLE_RATE));
        at
    }
}

/// Linear resampling of interleaved stereo to [`SAMPLE_RATE`].
#[derive(Default)]
struct Resampler {
    position: f64,
    last: [f32; 2],
}

impl Resampler {
    fn run(&mut self, rate: u32, samples: Vec<f32>) -> Vec<f32> {
        if rate == SAMPLE_RATE || rate == 0 || samples.len() < 2 {
            return samples;
        }
        let input: Vec<[f32; 2]> = samples.as_chunks::<2>().0.to_vec();
        let step = f64::from(rate) / f64::from(SAMPLE_RATE);
        let last = self.last;
        let frame = |i: isize| if i < 0 { last } else { input[i as usize] };
        let mut out = Vec::with_capacity((input.len() as f64 / step) as usize * 2 + 2);
        while self.position < (input.len() - 1) as f64 {
            let i = self.position.floor();
            let t = (self.position - i) as f32;
            let (a, b) = (frame(i as isize), frame(i as isize + 1));
            out.extend([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.position += step;
        }
        self.position -= input.len() as f64;
        self.last = input[input.len() - 1];
        out
    }
}

/// The helper to run: a program and the arguments before the page's.
#[derive(Clone)]
pub(crate) struct Helper {
    program: PathBuf,
    prefix: Vec<String>,
}

impl Helper {
    /// `$ISOCAST_BROWSER_HELPER`, or `isocast-browser.exe` next to isocastd.
    pub(crate) fn installed() -> Result<Self> {
        if let Some(path) = std::env::var_os("ISOCAST_BROWSER_HELPER") {
            return Ok(Self {
                program: PathBuf::from(path),
                prefix: Vec::new(),
            });
        }
        let exe = std::env::current_exe().context("locating isocastd")?;
        let path = exe.with_file_name("isocast-browser.exe");
        if !path.is_file() {
            bail!(
                "browser sources need isocast-browser.exe next to isocastd ({}); reinstall Isocast",
                path.display()
            );
        }
        Ok(Self {
            program: path,
            prefix: Vec::new(),
        })
    }
}

/// What a helper that failed before starting the page said, as advice.
fn explain_start_failure(status: Option<i32>, reason: Option<&str>, stderr: &str) -> String {
    match reason {
        // The helper's own words name the fix (the runtime's download link).
        Some(reason) if reason.contains("WebView2 Runtime") => format!("browser source: {reason}"),
        Some(reason) => format!("browser source could not start its page: {reason}"),
        None => {
            let tail = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let status = match status {
                // STATUS_DLL_NOT_FOUND, STATUS_ENTRYPOINT_NOT_FOUND
                Some(code) if matches!(code as u32, 0xC000_0135 | 0xC000_0139) => {
                    "a system library is missing; reinstall Isocast".to_string()
                }
                Some(code) => format!("status {code:#x}"),
                None => "killed".to_string(),
            };
            format!(
                "browser source helper exited ({status}) before loading the page{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            )
        }
    }
}

/// A kill-on-close job holding the helper and every process it starts.
struct Job(Owned<HANDLE>);

// SAFETY: an owned, unnamed kernel job, never inherited or exposed to a child.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    fn new() -> Result<Self> {
        let job = Job(unsafe { Owned::new(CreateJobObjectW(None, None)?) });
        unsafe { SetHandleInformation(*job.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
            .context("could not make the browser helper's job non-inheritable")?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                *job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .context("could not configure the browser helper's job")?;
        Ok(job)
    }

    fn kill(&self) {
        let _ = unsafe { TerminateJobObject(*self.0, 1) };
    }

    /// The working sets of every process in the job, in MiB.
    fn memory_mb(&self) -> u64 {
        // A fixed-size list: a header and room for 256 process ids.
        #[repr(C)]
        struct List {
            header: JOBOBJECT_BASIC_PROCESS_ID_LIST,
            more: [usize; 255],
        }
        let mut list = List {
            header: JOBOBJECT_BASIC_PROCESS_ID_LIST::default(),
            more: [0; 255],
        };
        if unsafe {
            QueryInformationJobObject(
                Some(*self.0),
                JobObjectBasicProcessIdList,
                (&mut list as *mut List).cast(),
                std::mem::size_of::<List>() as u32,
                None,
            )
        }
        .is_err()
        {
            return 0;
        }
        let count = list.header.NumberOfProcessIdsInList as usize;
        let first = list.header.ProcessIdList.as_ptr();
        let mut total = 0u64;
        for i in 0..count.min(256) {
            let pid = unsafe { *first.add(i) } as u32;
            let Ok(process) = (unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                    false,
                    pid,
                )
            }) else {
                continue;
            };
            let mut counters = PROCESS_MEMORY_COUNTERS::default();
            if unsafe {
                GetProcessMemoryInfo(
                    process,
                    &mut counters,
                    std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                )
            }
            .is_ok()
            {
                total += counters.WorkingSetSize as u64;
            }
            let _ = unsafe { CloseHandle(process) };
        }
        total / (1024 * 1024)
    }
}

fn memory_cap_mb() -> u64 {
    std::env::var("ISOCAST_BROWSER_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v >= 128)
        .unwrap_or(MEMORY_CAP_MB)
}

/// One page's settings, owned.
#[derive(Clone)]
struct Spec {
    /// Size, rate and audio: nothing private.
    args: Vec<String>,
    /// The page and its CSS, the first line on the helper's stdin: a URL
    /// can carry a token, and other processes can read a process's
    /// command line.
    page: String,
    width: u32,
    height: u32,
}

impl Spec {
    fn new(page: &BrowserPage<'_>, audio: bool) -> Self {
        let location = match page.location {
            PageLocation::Url(url) => protocol::Location::Url(url.into()),
            PageLocation::File(path) => {
                protocol::Location::File(path.to_string_lossy().into_owned().into_bytes())
            }
        };
        let mut args: Vec<String> = Vec::new();
        args.extend([
            "--width".into(),
            page.width.to_string(),
            "--height".into(),
            page.height.to_string(),
            "--fps".into(),
            page.fps.to_string(),
        ]);
        if audio {
            args.push("--audio".into());
        }
        Self {
            args,
            page: protocol::page_line(&location, page.css),
            width: page.width,
            height: page.height,
        }
    }
}

/// A started helper, in its own job.
struct Process {
    child: Child,
    job: Job,
    started: Instant,
    stderr: Arc<Mutex<String>>,
}

impl Process {
    fn spawn(helper: &Helper, spec: &Spec) -> Result<(Self, ChildStdin, ChildStdout)> {
        let job = Job::new()?;
        let mut child = Command::new(&helper.program)
            .args(&helper.prefix)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .with_context(|| format!("starting {}", helper.program.display()))?;
        // The helper starts WebView2's browser only after its first lines
        // of setup, so its children land in the job too.
        if let Err(err) = unsafe { AssignProcessToJobObject(*job.0, HANDLE(child.as_raw_handle())) }
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(err)).context(
                "could not put the browser helper in its kill-on-close job; nested jobs may be unavailable",
            );
        }
        let mut stdin = child.stdin.take().context("helper stdin")?;
        // The page first (a helper that died at once just won't read it).
        let _ = stdin
            .write_all(spec.page.as_bytes())
            .and_then(|_| stdin.flush());
        let stdout = child.stdout.take().context("helper stdout")?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let keep = stderr.clone();
            std::thread::Builder::new()
                .name("browser-helper-log".into())
                .spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        isocast_diagnostics::diagnostic!("info"; "isocast-browser: {line}");
                        let mut kept = keep.lock().unwrap_or_else(|e| e.into_inner());
                        if kept.len() < 8192 {
                            kept.push_str(&line);
                            kept.push('\n');
                        }
                    }
                })
                .context("starting the browser helper log thread")?;
        }
        Ok((
            Self {
                child,
                job,
                started: Instant::now(),
                stderr,
            },
            stdin,
            stdout,
        ))
    }

    fn stop(&mut self) {
        self.job.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the reader thread shares with the control and watchdog.
struct Shared {
    stdin: Mutex<Option<ChildStdin>>,
    loaded: AtomicBool,
    stop: AtomicBool,
    /// Host-clock nanos of the last picture or sign of life (or of the
    /// helper's start).
    last_frame: AtomicU64,
    restarts: AtomicU64,
}

impl Shared {
    fn command(&self, line: &str) {
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipe) = stdin.as_mut()
            && writeln!(pipe, "{line}").and_then(|_| pipe.flush()).is_err()
        {
            *stdin = None;
        }
    }
}

struct Control(Arc<Shared>);

impl BrowserControl for Control {
    fn refresh(&self) {
        self.0.command("reload");
    }

    fn set_loaded(&self, loaded: bool) {
        self.0.loaded.store(loaded, Ordering::SeqCst);
        self.0.command(if loaded { "load" } else { "unload" });
    }
}

/// A running browser source; dropping it stops the helper.
pub struct BrowserCapture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for BrowserCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Closing stdin ends the helper; the thread kills it if it lingers.
        *self.shared.stdin.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn now_nanos() -> u64 {
    crate::now().as_nanos().max(0) as u64
}

/// Start rendering `page` in `helper`, delivering pictures drawn into
/// `pool`. Fails, with the fix, if the helper can't load the page at all
/// (no WebView2 Runtime, no desktop to capture on).
pub(crate) fn start<P>(
    helper: Helper,
    page: &BrowserPage<'_>,
    pool: P,
    sink: VideoSink<P::Frame>,
    audio: Option<AudioSink>,
) -> Result<(BrowserCapture, CaptureFormat, Arc<dyn BrowserControl>)>
where
    P: FramePool + 'static,
    P::Frame: Send + 'static,
{
    let spec = Spec::new(page, audio.is_some());
    let shared = Arc::new(Shared {
        stdin: Mutex::new(None),
        loaded: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        last_frame: AtomicU64::new(now_nanos()),
        restarts: AtomicU64::new(0),
    });
    // The first start is checked here, so a broken install is an error the
    // caller sees, not a blank source.
    let (ready, started) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let thread = std::thread::Builder::new()
        .name("browser-source".into())
        .spawn({
            let shared = shared.clone();
            move || {
                let first = start_helper(&helper, &spec);
                let (process, stdin, reader) = match first {
                    Ok(started) => started,
                    Err(err) => {
                        let _ = ready.send(Err(err));
                        return;
                    }
                };
                *shared.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(stdin);
                let _ = ready.send(Ok(()));
                run(helper, spec, shared, process, reader, pool, sink, audio)
            }
        })
        .context("starting the browser source thread")?;
    match started.recv_timeout(START_TIMEOUT + Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = thread.join();
            return Err(err);
        }
        Err(_) => {
            // wait_ready kills a helper that doesn't start in time; this
            // only covers the spawn itself hanging.
            shared.stop.store(true, Ordering::SeqCst);
            bail!(
                "browser source helper didn't start the page within {} s",
                START_TIMEOUT.as_secs()
            );
        }
    }
    Ok((
        BrowserCapture {
            shared: shared.clone(),
            thread: Some(thread),
        },
        CaptureFormat {
            width: page.width,
            height: page.height,
            fps: page.fps,
        },
        Arc::new(Control(shared)),
    ))
}

type Reader = BufReader<ChildStdout>;

fn start_helper(helper: &Helper, spec: &Spec) -> Result<(Process, ChildStdin, Reader)> {
    let (mut process, stdin, stdout) = Process::spawn(helper, spec)?;
    let mut reader = BufReader::with_capacity(1 << 20, stdout);
    wait_ready(&mut process, &mut reader)?;
    Ok((process, stdin, reader))
}

/// Wait for the helper's `REDY` (or its failure).
fn wait_ready(process: &mut Process, reader: &mut Reader) -> Result<()> {
    let (send, receive) = std::sync::mpsc::channel();
    // Read the first record on a scoped thread so the start can time out.
    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = (|| -> Result<Result<(), String>> {
                match protocol::read_header(reader)? {
                    Some(protocol::Header::Ready) => Ok(Ok(())),
                    Some(protocol::Header::Fail { len }) => {
                        let mut reason = vec![0; len];
                        reader.read_exact(&mut reason)?;
                        Ok(Err(String::from_utf8_lossy(&reason).into_owned()))
                    }
                    Some(other) => bail!("browser helper started with {other:?}"),
                    None => Ok(Err(String::new())),
                }
            })();
            let _ = send.send(result);
        });
        let result = receive.recv_timeout(START_TIMEOUT);
        if result.is_err() {
            // A hung start: killing it ends the reader's read.
            process.stop();
        }
        result
    });
    match outcome {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(reason))) => {
            let status = process.child.wait().ok().and_then(|s| s.code());
            std::thread::sleep(Duration::from_millis(50)); // let stderr arrive
            let stderr = process
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let reason = (!reason.is_empty()).then_some(reason.as_str());
            Err(anyhow!(explain_start_failure(status, reason, &stderr)))
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err(anyhow!(
            "browser source helper didn't start the page within {} s",
            START_TIMEOUT.as_secs()
        )),
    }
}

/// Read pictures and sound until stopped, restarting the helper after a
/// crash, a hang or too much memory.
#[allow(clippy::too_many_arguments)]
fn run<P>(
    helper: Helper,
    spec: Spec,
    shared: Arc<Shared>,
    mut process: Process,
    reader: Reader,
    mut pool: P,
    sink: VideoSink<P::Frame>,
    audio: Option<AudioSink>,
) where
    P: FramePool,
{
    let audio = audio.map(|sink| (sink, Mutex::new((Stamper::default(), Resampler::default()))));
    let mut failures = 0usize;
    let mut reader = Some(reader);
    loop {
        let input = reader.take().expect("a started helper has a reader");
        shared.last_frame.store(now_nanos(), Ordering::SeqCst);
        let ended = AtomicBool::new(false);
        let why = std::thread::scope(|scope| {
            let reading = scope.spawn(|| {
                let result = read(input, &spec, &shared, &mut pool, &sink, audio.as_ref());
                ended.store(true, Ordering::SeqCst);
                result
            });
            // The watchdog: stop, hang, memory.
            let cap = memory_cap_mb();
            let mut checked = Instant::now();
            let why = loop {
                std::thread::sleep(Duration::from_millis(100));
                if ended.load(Ordering::SeqCst) {
                    break None;
                }
                if shared.stop.load(Ordering::SeqCst) {
                    break Some("stopped".to_string());
                }
                let idle = Duration::from_nanos(
                    now_nanos().saturating_sub(shared.last_frame.load(Ordering::SeqCst)),
                );
                if shared.loaded.load(Ordering::SeqCst) && idle > HANG_TIMEOUT {
                    break Some(format!(
                        "no picture for {} s (the page or WebView2 hung)",
                        idle.as_secs()
                    ));
                }
                if checked.elapsed() > Duration::from_secs(2) {
                    checked = Instant::now();
                    let used = process.job.memory_mb();
                    if used > cap {
                        break Some(format!("it used {used} MB, over the {cap} MB cap"));
                    }
                }
            };
            // Ending the helper ends the reader.
            let status = process.child.try_wait().ok().flatten();
            process.stop();
            let read_result = reading.join();
            let status = status
                .or_else(|| process.child.try_wait().ok().flatten())
                .map_or("killed".into(), |s| s.to_string());
            why.or_else(|| match read_result {
                Ok(Ok(())) => Some(format!("the helper closed its output ({status})")),
                Ok(Err(err)) => Some(format!("{err:#} ({status})")),
                Err(_) => Some(format!("reading the helper panicked ({status})")),
            })
        });
        let why = why.unwrap_or_default();
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if process.started.elapsed() > STABLE {
            failures = 0;
        }
        let delay = BACKOFF[failures.min(BACKOFF.len() - 1)];
        failures += 1;
        shared.restarts.fetch_add(1, Ordering::SeqCst);
        isocast_diagnostics::diagnostic!("warn";
            "isocastd: browser source restarting its page in {} s: {why}", delay.as_secs()
        );
        // Wait out the backoff, still stoppable.
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match start_helper(&helper, &spec) {
            Ok((started, stdin, input)) => {
                *shared.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(stdin);
                if !shared.loaded.load(Ordering::SeqCst) {
                    shared.command("unload");
                }
                process = started;
                reader = Some(input);
            }
            Err(err) => {
                // Not even starting (uninstalled meanwhile?): the source stops.
                (sink.stopped)(format!("{err:#}"));
                return;
            }
        }
    }
}

/// Pictures and sound until the helper's output ends or breaks.
fn read<P: FramePool>(
    mut reader: impl Read,
    spec: &Spec,
    shared: &Shared,
    pool: &mut P,
    sink: &VideoSink<P::Frame>,
    audio: Option<&(AudioSink, Mutex<(Stamper, Resampler)>)>,
) -> Result<()> {
    let mut discard = Vec::new();
    loop {
        let Some(header) = protocol::read_header(&mut reader)? else {
            return Ok(());
        };
        match header {
            // A beat: the page is loading or still, without new pictures.
            protocol::Header::Ready => shared.last_frame.store(now_nanos(), Ordering::SeqCst),
            protocol::Header::Frame { width, height } => {
                let len = width as usize * height as usize * 4;
                if (width, height) != (spec.width, spec.height) {
                    bail!(
                        "browser helper sent {width}x{height} pictures, not {}x{}",
                        spec.width,
                        spec.height
                    );
                }
                let row = width as usize * 4;
                let mut failed = None;
                let frame = pool.draw(&mut |pixels, stride| {
                    let result = if stride == row {
                        reader.read_exact(&mut pixels[..len])
                    } else {
                        (0..height as usize).try_for_each(|y| {
                            reader.read_exact(&mut pixels[y * stride..y * stride + row])
                        })
                    };
                    if let Err(err) = result {
                        failed = Some(err);
                    }
                })?;
                if let Some(err) = failed {
                    return Err(err).context("reading a browser picture");
                }
                match frame {
                    Some(frame) => {
                        shared.last_frame.store(now_nanos(), Ordering::SeqCst);
                        (sink.frame)(frame, crate::now());
                    }
                    None => {
                        // Every pooled frame is still downstream: skip it.
                        discard.resize(len, 0);
                        reader
                            .read_exact(&mut discard)
                            .context("reading a browser picture")?;
                        shared.last_frame.store(now_nanos(), Ordering::SeqCst);
                    }
                }
            }
            protocol::Header::Audio { rate, len } => {
                let mut bytes = vec![0u8; len];
                reader
                    .read_exact(&mut bytes)
                    .context("reading browser audio")?;
                let now = crate::now();
                if let Some((sink, state)) = audio {
                    let samples: Vec<f32> = bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b))
                        .map(|s| if s.is_finite() { s } else { 0.0 })
                        .collect();
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    let samples = state.1.run(rate, samples);
                    let frames = samples.len() / 2;
                    if frames > 0 {
                        let at = state.0.stamp(frames, now);
                        (sink.samples)(&samples, at);
                    }
                }
            }
            protocol::Header::Fail { len } => {
                let mut reason = vec![0; len];
                reader.read_exact(&mut reason)?;
                bail!("{}", String::from_utf8_lossy(&reason));
            }
        }
    }
}

#[cfg(test)]
mod tests;
