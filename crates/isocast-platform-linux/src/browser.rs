//! Browser sources: each page renders in its own `isocast-browser` helper
//! process (WebKitGTK 6 on a private GTK Broadway display; see
//! crates/isocast-browser), which streams pictures and sound back over a
//! pipe. WebKit never runs inside isocastd: a missing library fails only
//! the source, with the packages to install; a crash, a hang or a page that
//! grows past the memory cap restarts the helper (with backoff) while the
//! engine and its other sources keep running.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{
    AudioSink, BrowserAudioStamper as Stamper, BrowserControl, BrowserPage, CaptureFormat,
    FramePool as _, PageLocation, SAMPLE_RATE, VideoSink,
};

use crate::frames::{Frame, FramePool};
use crate::system;

/// How long a new helper may take to say it is loading the page.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// No picture for this long from a loaded page: the helper hung.
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
/// The helper and its WebKit processes together (resident memory); more,
/// and the page is restarted. `ISOCAST_BROWSER_MEMORY_MB` overrides it.
const MEMORY_CAP_MB: u64 = 1536;
const PACKAGES: &str = "install libwebkitgtk-6.0-4 and libgtk-4-bin (Debian/Ubuntu), \
     webkitgtk6.0 and gtk4 (Fedora) or webkitgtk-6.0 and gtk4 (Arch)";

/// The helper's records (crates/isocast-browser/src/main.rs).
pub(crate) mod protocol {
    use std::io::Read;

    use anyhow::{Context, Result, bail};

    /// Largest payload accepted: an 8192×8192 picture and its header.
    pub const MAX_PAYLOAD: u32 = 8192 * 8192 * 4 + 8;
    /// Audio sample rates accepted. The page's script chooses the rate,
    /// so it is untrusted: resampling a rate of 1 to 48 kHz would size its
    /// output at 48,000 times the input.
    pub const AUDIO_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
    /// Largest audio payload: over a second at the highest rate (the
    /// helper sends at most a second per record).
    pub const MAX_AUDIO: u32 = 2 << 20;
    /// Largest failure reason.
    pub const MAX_FAIL: u32 = 4 << 10;

    #[derive(Debug, PartialEq, Eq)]
    pub enum Header {
        Ready,
        /// A picture of this size; its pixels follow.
        Frame {
            width: u32,
            height: u32,
        },
        /// Interleaved stereo f32 at this rate: `len` bytes follow.
        Audio {
            rate: u32,
            len: usize,
        },
        /// A reason follows, `len` bytes.
        Fail {
            len: usize,
        },
    }

    /// Read the next record's header (and, for a frame or audio, its first
    /// fields); `None` at the end of the stream.
    pub fn read_header(input: &mut impl Read) -> Result<Option<Header>> {
        let mut head = [0u8; 8];
        match input.read_exact(&mut head) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err).context("reading the browser helper"),
        }
        let len = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes"));
        if len > MAX_PAYLOAD {
            bail!("browser helper sent an oversized record ({len} bytes)");
        }
        let mut u32_field = || -> Result<u32> {
            let mut field = [0u8; 4];
            input
                .read_exact(&mut field)
                .context("reading the browser helper")?;
            Ok(u32::from_le_bytes(field))
        };
        let limit = match &head[..4] {
            b"REDY" => 0,
            b"AUDI" => 4 + MAX_AUDIO,
            b"FAIL" => MAX_FAIL,
            _ => MAX_PAYLOAD,
        };
        if len > limit {
            bail!(
                "browser helper sent an oversized {} record ({len} bytes, at most {limit})",
                String::from_utf8_lossy(&head[..4])
            );
        }
        Ok(Some(match &head[..4] {
            b"REDY" => Header::Ready,
            b"FRAM" => {
                let (width, height) = (u32_field()?, u32_field()?);
                if len as u64 != 8 + u64::from(width) * u64::from(height) * 4 {
                    bail!(
                        "browser helper sent a picture whose size doesn't match ({width}x{height}, {len} bytes)"
                    );
                }
                Header::Frame { width, height }
            }
            b"AUDI" => {
                let rate = u32_field()?;
                let len = len
                    .checked_sub(4)
                    .context("browser helper sent a short audio record")?
                    as usize;
                if !len.is_multiple_of(8) {
                    bail!("browser helper sent audio that isn't whole stereo f32 frames");
                }
                if !AUDIO_RATES.contains(&rate) {
                    bail!(
                        "browser helper sent audio at {rate} Hz (accepted: {}..={} Hz)",
                        AUDIO_RATES.start(),
                        AUDIO_RATES.end()
                    );
                }
                Header::Audio { rate, len }
            }
            b"FAIL" => Header::Fail { len: len as usize },
            tag => bail!(
                "browser helper sent an unknown record {:?}",
                String::from_utf8_lossy(tag)
            ),
        }))
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
        if rate == SAMPLE_RATE || samples.len() < 2 {
            return samples;
        }
        // The protocol refuses other rates; never size a buffer from one.
        if !protocol::AUDIO_RATES.contains(&rate) {
            return Vec::new();
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

/// Where the helper is: `$ISOCAST_BROWSER_HELPER`, or next to isocastd.
fn helper_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ISOCAST_BROWSER_HELPER") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().context("locating isocastd")?;
    let path = exe.with_file_name("isocast-browser");
    if !path.is_file() {
        bail!("{}", missing_helper(&path).detail);
    }
    Ok(path)
}

/// Why there is no helper beside isocastd, and the way to get one. It is
/// built only where GTK is 4.10 or newer (WebKitGTK 6's Rust bindings need
/// it): the Debian 12 package (GTK 4.8) ships without it.
fn missing_helper(path: &Path) -> isocast_engine::DoctorWarning {
    isocast_engine::DoctorWarning {
        detail: format!(
            "browser sources aren't available: this Isocast has no browser helper ({} is \
             missing), which needs GTK 4.10 or newer (Ubuntu 24.04 or newer; Debian 12 has 4.8)",
            path.display()
        ),
        fix: "use Isocast on Ubuntu 24.04 or newer (its .deb or the tarball), or another system \
              with GTK 4.10+ and WebKitGTK 6"
            .into(),
    }
}

/// For `doctor`: what the installed helper says about its sandbox (run
/// with `--sandbox-status`, under the AppArmor label it gets at exec), or
/// why it can't run at all, or that there is no helper (the Debian 12
/// package, or a build without it). `None` when pages run sandboxed.
pub fn doctor_warning() -> Option<isocast_engine::DoctorWarning> {
    let path = match std::env::var_os("ISOCAST_BROWSER_HELPER") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()
            .ok()?
            .with_file_name("isocast-browser"),
    };
    if !path.is_file() {
        return Some(missing_helper(&path));
    }
    let output = run_with_timeout(
        Command::new(&path).arg("--sandbox-status"),
        Duration::from_secs(5),
    );
    sandbox_warning(output)
}

/// `--sandbox-status`'s stdout, or its exit status and stderr if it
/// failed; `Err` if it couldn't run or took too long.
fn run_with_timeout(
    command: &mut Command,
    limit: Duration,
) -> Result<std::process::Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + limit;
    while matches!(child.try_wait(), Ok(None)) {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("it didn't answer within {} s", limit.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().map_err(|e| e.to_string())
}

fn sandbox_warning(
    output: Result<std::process::Output, String>,
) -> Option<isocast_engine::DoctorWarning> {
    let output = match output {
        Ok(output) => output,
        Err(err) => {
            return Some(isocast_engine::DoctorWarning {
                detail: format!("isocast-browser couldn't be checked: {err}"),
                fix: "reinstall Isocast".into(),
            });
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Some(isocast_engine::DoctorWarning {
            detail: explain_start_failure(output.status.code(), None, &stderr),
            fix: PACKAGES.into(),
        });
    }
    let line = stdout.trim();
    let reason = line.strip_prefix("sandbox off: ")?;
    Some(isocast_engine::DoctorWarning {
        detail: format!(
            "pages run without WebKit's sandbox ({reason}); they still run in their own \
             processes, outside isocastd"
        ),
        fix: "install Isocast's .deb, which adds an AppArmor profile that lets isocast-browser \
              keep the sandbox"
            .into(),
    })
}

/// What a helper that failed before starting the page printed, as advice.
fn explain_start_failure(status: Option<i32>, reason: Option<&str>, stderr: &str) -> String {
    if stderr.contains("error while loading shared libraries") {
        let library = stderr
            .split("error while loading shared libraries: ")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .unwrap_or("a library");
        return format!(
            "browser sources need WebKitGTK 6 and GTK 4 ({library} is missing): {PACKAGES}"
        );
    }
    match reason {
        Some(reason) if reason.contains("gtk4-broadwayd") => {
            format!("browser source: {reason}")
        }
        Some(reason) => format!("browser source could not start its page: {reason}"),
        None => {
            let tail = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            format!(
                "browser source helper exited ({}) before loading the page{}",
                status.map_or("killed".into(), |code| format!("status {code}")),
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            )
        }
    }
}

/// The resident memory of `pid` and every process below it, in MiB.
fn tree_memory_mb(pid: u32) -> u64 {
    let mut parents: Vec<(u32, u32, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(p) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // pid (comm) state ppid ... ; comm may hold spaces.
            let Some(after) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
                continue;
            };
            let ppid = after
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let rss_kb = std::fs::read_to_string(entry.path().join("status"))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find_map(|l| l.strip_prefix("VmRSS:"))
                        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                })
                .unwrap_or(0);
            parents.push((p, ppid, rss_kb));
        }
    }
    let mut tree = vec![pid];
    let mut total = 0;
    let mut i = 0;
    while i < tree.len() {
        let current = tree[i];
        for &(p, ppid, rss) in &parents {
            if p == current {
                total += rss;
            }
            if ppid == current && !tree.contains(&p) {
                tree.push(p);
            }
        }
        i += 1;
    }
    total / 1024
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
    /// can carry a token, and a process's arguments are world-readable
    /// (/proc/<pid>/cmdline).
    page: String,
    width: u32,
    height: u32,
}

/// Bytes as lowercase hex (one word on the page line, whatever they hold).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Spec {
    fn new(page: &BrowserPage<'_>, audio: bool) -> Self {
        let (kind, location) = match page.location {
            PageLocation::Url(url) => ("url", url.as_bytes().to_vec()),
            PageLocation::File(path) => (
                "file",
                std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec(),
            ),
        };
        let page_line = format!(
            "page {kind} {} {}\n",
            hex(&location),
            hex(page.css.trim().as_bytes())
        );
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
            page: page_line,
            width: page.width,
            height: page.height,
        }
    }
}

/// A started helper.
struct Helper {
    child: Child,
    started: Instant,
    stderr: Arc<Mutex<String>>,
}

impl Helper {
    fn spawn(path: &Path, spec: &Spec) -> Result<(Self, ChildStdin, std::process::ChildStdout)> {
        let mut command = Command::new(path);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The helper (and so its display and WebKit) dies with isocastd.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", path.display()))?;
        let mut stdin = child.stdin.take().context("helper stdin")?;
        let stdout = child.stdout.take().context("helper stdout")?;
        // A helper that dies first explains itself on stdout and stderr.
        let _ = stdin
            .write_all(spec.page.as_bytes())
            .and_then(|_| stdin.flush());
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(err) = child.stderr.take() {
            let keep = stderr.clone();
            std::thread::Builder::new()
                .name("browser-helper-log".into())
                .spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        // The helper marks what degrades a source.
                        let level = if line.starts_with("warning: ") {
                            "warn"
                        } else {
                            "info"
                        };
                        isocast_diagnostics::diagnostic(
                            level,
                            module_path!(),
                            format!("isocast-browser: {line}"),
                        );
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
                started: Instant::now(),
                stderr,
            },
            stdin,
            stdout,
        ))
    }

    /// Give it `grace` to exit by itself (its stdin closed, it removes its
    /// private files), then kill it.
    fn stop(&mut self, grace: Duration) {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
    }
}

/// How long a stopping source's helper may take to exit by itself.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// What the reader thread shares with the control and watchdog.
struct Shared {
    stdin: Mutex<Option<ChildStdin>>,
    loaded: AtomicBool,
    stop: AtomicBool,
    /// Host-clock nanos of the last picture (or of the helper's start).
    last_frame: AtomicU64,
    restarts: AtomicU64,
    /// Why the helper was last restarted.
    last_restart: Mutex<String>,
    /// The helper and its WebKit processes' memory cap, in MB.
    memory_cap_mb: u64,
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
        // The hang clock starts again: the page is loading anew.
        self.0.last_frame.store(now_nanos(), Ordering::SeqCst);
        self.0.command("reload");
    }

    fn set_loaded(&self, loaded: bool) {
        // Back on air: the hang clock starts now, before `loaded` turns it
        // on, so time spent off air (no pictures, by design) never counts.
        if loaded {
            self.0.last_frame.store(now_nanos(), Ordering::SeqCst);
        }
        self.0.loaded.store(loaded, Ordering::SeqCst);
        self.0.command(if loaded { "load" } else { "unload" });
    }
}

/// A running browser source; dropping it stops the helper.
pub struct Capture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
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
    system::now().as_nanos().max(0) as u64
}

/// Start rendering `page` in a helper. Fails, with the fix, if the helper
/// can't load the page at all (missing libraries, no display daemon).
pub fn capture_browser(
    page: &BrowserPage<'_>,
    sink: VideoSink<Frame>,
    audio: Option<AudioSink>,
) -> Result<(Capture, CaptureFormat, Arc<dyn BrowserControl>)> {
    start(helper_path()?, page, sink, audio)
}

fn start(
    path: PathBuf,
    page: &BrowserPage<'_>,
    sink: VideoSink<Frame>,
    audio: Option<AudioSink>,
) -> Result<(Capture, CaptureFormat, Arc<dyn BrowserControl>)> {
    start_capped(path, page, sink, audio, memory_cap_mb())
}

fn start_capped(
    path: PathBuf,
    page: &BrowserPage<'_>,
    sink: VideoSink<Frame>,
    audio: Option<AudioSink>,
    memory_cap_mb: u64,
) -> Result<(Capture, CaptureFormat, Arc<dyn BrowserControl>)> {
    let spec = Spec::new(page, audio.is_some());
    let pool = FramePool::new(page.width, page.height)?;
    let shared = Arc::new(Shared {
        stdin: Mutex::new(None),
        loaded: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        last_frame: AtomicU64::new(now_nanos()),
        restarts: AtomicU64::new(0),
        last_restart: Mutex::new(String::new()),
        memory_cap_mb,
    });
    // The first start is checked here, so a broken install is an error the
    // caller sees, not a blank source. Every helper is spawned by the
    // source's own thread: PR_SET_PDEATHSIG fires when the spawning
    // *thread* ends, and the engine opens sources on short-lived threads.
    let (ready, started) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let thread = std::thread::Builder::new()
        .name("browser-source".into())
        .spawn({
            let shared = shared.clone();
            move || {
                let first = Helper::spawn(&path, &spec).and_then(|(mut helper, stdin, stdout)| {
                    let mut reader = BufReader::with_capacity(1 << 20, stdout);
                    wait_ready(&mut helper, &mut reader, &shared.stop)?;
                    Ok((helper, stdin, reader))
                });
                let (helper, stdin, reader) = match first {
                    Ok(started) => started,
                    Err(err) => {
                        let _ = ready.send(Err(err));
                        return;
                    }
                };
                *shared.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(stdin);
                let _ = ready.send(Ok(()));
                let session = Session {
                    helper,
                    reader: Some(reader),
                };
                run(path, spec, shared, session, pool, sink, audio)
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
        Capture {
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

struct Session {
    helper: Helper,
    reader: Option<BufReader<std::process::ChildStdout>>,
}

/// Wait for the helper's `REDY` (or its failure).
/// Gives up early, killing the helper, once `stop` is set (the source is
/// being dropped).
fn wait_ready(
    helper: &mut Helper,
    reader: &mut BufReader<std::process::ChildStdout>,
    stop: &AtomicBool,
) -> Result<()> {
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
        // In short slices, so a source dropped meanwhile isn't held up.
        let deadline = Instant::now() + START_TIMEOUT;
        let result = loop {
            match receive.recv_timeout(Duration::from_millis(100)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    if Instant::now() < deadline && !stop.load(Ordering::SeqCst) => {}
                other => break other,
            }
        };
        if result.is_err() {
            // A hung start, or the source stopping: killing it ends the
            // reader's read.
            let _ = helper.child.kill();
        }
        result
    });
    match outcome {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(reason))) => {
            // It normally exits right after; one that lingers is killed,
            // so this never waits on it forever.
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline && matches!(helper.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = helper.child.kill();
            let status = helper.child.wait().ok().and_then(|s| s.code());
            std::thread::sleep(Duration::from_millis(50)); // let stderr arrive
            let stderr = helper
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let reason = (!reason.is_empty()).then_some(reason.as_str());
            Err(anyhow!(explain_start_failure(status, reason, &stderr)))
        }
        Ok(Err(err)) => Err(err),
        Err(_) if stop.load(Ordering::SeqCst) => Err(anyhow!("browser source stopped")),
        Err(_) => Err(anyhow!(
            "browser source helper didn't start the page within {} s",
            START_TIMEOUT.as_secs()
        )),
    }
}

/// Read pictures and sound until stopped, restarting the helper after a
/// crash, a hang or too much memory.
fn run(
    path: PathBuf,
    spec: Spec,
    shared: Arc<Shared>,
    mut session: Session,
    mut pool: FramePool,
    sink: VideoSink<Frame>,
    audio: Option<AudioSink>,
) {
    let mut failures = 0usize;
    loop {
        let reader = session
            .reader
            .take()
            .expect("a started session has a reader");
        let pid = session.helper.child.id();
        shared.last_frame.store(now_nanos(), Ordering::SeqCst);
        let ended = AtomicBool::new(false);
        let why = std::thread::scope(|scope| {
            let reading = scope.spawn(|| {
                let result = read(reader, &spec, &shared, &mut pool, &sink, audio.as_ref());
                ended.store(true, Ordering::SeqCst);
                result
            });
            // The watchdog: stop, hang, memory.
            let cap = shared.memory_cap_mb;
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
                        "no picture for {} s (the page or WebKit hung)",
                        idle.as_secs()
                    ));
                }
                if checked.elapsed() > Duration::from_secs(2) {
                    checked = Instant::now();
                    let used = tree_memory_mb(pid);
                    if used > cap {
                        break Some(format!("it used {used} MB, over the {cap} MB cap"));
                    }
                }
            };
            // Ending the helper ends the reader. A source being stopped
            // has closed its stdin: the helper gets a moment to clean up.
            // A crashed, hung or oversized one is killed at once.
            let stopping = shared.stop.load(Ordering::SeqCst) || ended.load(Ordering::SeqCst);
            session
                .helper
                .stop(if stopping { EXIT_GRACE } else { Duration::ZERO });
            let read_result = reading.join();
            let status = session
                .helper
                .child
                .try_wait()
                .ok()
                .flatten()
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
        if session.helper.started.elapsed() > STABLE {
            failures = 0;
        }
        let delay = BACKOFF[failures.min(BACKOFF.len() - 1)];
        failures += 1;
        *shared
            .last_restart
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = why.clone();
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
        match Helper::spawn(&path, &spec).and_then(|(mut helper, stdin, stdout)| {
            let mut reader = BufReader::with_capacity(1 << 20, stdout);
            wait_ready(&mut helper, &mut reader, &shared.stop)?;
            Ok((helper, stdin, reader))
        }) {
            Ok((helper, stdin, reader)) => {
                *shared.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(stdin);
                if !shared.loaded.load(Ordering::SeqCst) {
                    shared.command("unload");
                }
                session = Session {
                    helper,
                    reader: Some(reader),
                };
            }
            Err(err) => {
                // Not even starting (uninstalled meanwhile?): the source
                // stops, with the reason, unless it is being dropped.
                if !shared.stop.load(Ordering::SeqCst) {
                    (sink.stopped)(format!("{err:#}"));
                }
                return;
            }
        }
    }
}

/// Pictures and sound until the helper's output ends or breaks.
fn read(
    mut reader: impl Read,
    spec: &Spec,
    shared: &Shared,
    pool: &mut FramePool,
    sink: &VideoSink<Frame>,
    audio: Option<&AudioSink>,
) -> Result<()> {
    // Each new reader is a new helper process, including watchdog restarts.
    let (mut stamper, mut resampler) = (Stamper::default(), Resampler::default());
    let mut discard = Vec::new();
    loop {
        let Some(header) = protocol::read_header(&mut reader)? else {
            return Ok(());
        };
        match header {
            // Sent while the page loads: alive, just no picture yet.
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
                let mut failed = None;
                let frame = pool.draw(&mut |pixels, _| {
                    if let Err(err) = reader.read_exact(&mut pixels[..len]) {
                        failed = Some(err);
                    }
                })?;
                if let Some(err) = failed {
                    return Err(err).context("reading a browser picture");
                }
                match frame {
                    Some(frame) => {
                        shared.last_frame.store(now_nanos(), Ordering::SeqCst);
                        (sink.frame)(frame, system::now());
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
                let now = system::now();
                if let Some(sink) = audio {
                    if len == 0 {
                        // Document-start marker, ordered with the page's audio.
                        (stamper, resampler) = Default::default();
                        continue;
                    }
                    let samples: Vec<f32> = bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32::from_le_bytes(*b))
                        .map(|s| if s.is_finite() { s } else { 0.0 })
                        .collect();
                    let samples = resampler.run(rate, samples);
                    let frames = samples.len() / 2;
                    if frames > 0 {
                        let (at, gap) = stamper.stamp(frames, now);
                        if let Some(gap) = gap {
                            (sink.report_gap)(at, gap);
                        }
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
