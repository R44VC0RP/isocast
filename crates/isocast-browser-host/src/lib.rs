//! isocastd's side of a browser source, on every OS: start an
//! `isocast-browser` helper process for the page, read its pictures and
//! sound (crates/isocast-browser-protocol), and keep it running. The page's
//! browser engine never runs inside isocastd: a helper that can't start
//! fails only its source, with the helper's own advice; a crash, a hang or
//! a page that grows past the memory cap restarts the helper with backoff
//! while the engine and its other sources keep running.
//!
//! The platform ([`HelperOs`]) supplies the helper's command, confines the
//! started process and everything it starts (so they all end with the
//! source or with isocastd, and their memory is counted together), and the
//! host clock. Windows uses this (a kill-on-close job); Linux still has its
//! own copy (crates/isocast-platform-linux/src/browser.rs) until it moves
//! here.

use std::io::{BufRead, BufReader, Read, Write};
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

/// How far the page's audio may drift from the host clock before its
/// timestamps are reset to it (as on macOS).
const AUDIO_RESYNC: Duration = Duration::from_millis(150);
/// How often waits check whether the source was stopped.
const POLL: Duration = Duration::from_millis(100);

/// What the platform does for the host.
pub trait HelperOs: Send + Sync + 'static {
    /// The helper's command: its program and any arguments before the
    /// page's, with the OS's process options (no console window, death with
    /// isocastd). The host sets stdin, stdout and stderr.
    fn command(&self) -> Command;
    /// Confine a just-started helper and everything it will start. On an
    /// error the host kills the child and the source fails.
    fn adopt(&self, child: &Child) -> Result<Box<dyn Confinement>>;
    /// The host clock that pictures and sound are stamped on.
    fn now(&self) -> Time;
    /// Advice from a helper that ended before loading the page: its exit
    /// code, its `FAIL` reason if it sent one, and the end of its stderr.
    fn explain_start_failure(
        &self,
        status: Option<i32>,
        reason: Option<&str>,
        stderr: &str,
    ) -> String;
}

/// A started helper's process tree.
pub trait Confinement: Send + Sync {
    /// End the helper and every process it started.
    fn kill(&self);
    /// The resident memory of the helper and its processes, in MiB.
    fn memory_mb(&self) -> u64;
}

/// Limits and delays (shorter ones in tests).
#[derive(Clone, Debug)]
pub struct Timing {
    /// How long a new helper may take to say it is loading the page.
    pub start: Duration,
    /// No picture or beat for this long from a loaded page: the helper hung.
    pub hang: Duration,
    /// Restart delays after a failure, the last repeated.
    pub backoff: Vec<Duration>,
    /// A helper that ran this long before failing starts the backoff afresh.
    pub stable: Duration,
    /// The helper and its processes together; more, and it's restarted.
    pub memory_cap_mb: u64,
    /// Frame pools that fail in a row (a GPU upload that broke) before the
    /// source stops: a new pool can't fix a lost device.
    pub pool_failures: usize,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(30),
            hang: Duration::from_secs(10),
            backoff: [1, 2, 4, 8, 30].map(Duration::from_secs).to_vec(),
            stable: Duration::from_secs(60),
            memory_cap_mb: std::env::var("ISOCAST_BROWSER_MEMORY_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&v| v >= 128)
                .unwrap_or(1536),
            pool_failures: 3,
        }
    }
}

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

/// One page's settings, owned.
#[derive(Clone)]
struct Spec {
    /// Size, rate and audio: nothing private.
    args: Vec<String>,
    /// The page and its CSS, the first line on the helper's stdin: a URL
    /// can carry a token, and other processes can read a command line.
    page: String,
    width: u32,
    height: u32,
}

impl Spec {
    fn new(page: &BrowserPage<'_>, audio: bool) -> Self {
        let location = match page.location {
            PageLocation::Url(url) => protocol::Location::Url(url.into()),
            #[cfg(unix)]
            PageLocation::File(path) => protocol::Location::File(
                std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec(),
            ),
            #[cfg(not(unix))]
            PageLocation::File(path) => {
                protocol::Location::File(path.to_string_lossy().into_owned().into_bytes())
            }
        };
        let mut args: Vec<String> = vec![
            "--width".into(),
            page.width.to_string(),
            "--height".into(),
            page.height.to_string(),
            "--fps".into(),
            page.fps.to_string(),
        ];
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

/// A started helper, confined.
struct Process {
    child: Child,
    tree: Box<dyn Confinement>,
    started: Instant,
    stderr: Arc<Mutex<String>>,
}

impl Process {
    fn spawn(os: &dyn HelperOs, spec: &Spec) -> Result<(Self, ChildStdin, ChildStdout)> {
        let mut command = os.command();
        let program = command.get_program().to_string_lossy().into_owned();
        let mut child = command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {program}"))?;
        let tree = match os.adopt(&child) {
            Ok(tree) => tree,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
        };
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
                tree,
                started: Instant::now(),
                stderr,
            },
            stdin,
            stdout,
        ))
    }

    fn stop(&mut self) {
        self.tree.kill();
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
    os: Arc<dyn HelperOs>,
    stdin: Mutex<Option<ChildStdin>>,
    loaded: AtomicBool,
    stop: AtomicBool,
    /// Host-clock nanos of the last picture or beat (or of the helper's
    /// start, or of the page being loaded again).
    last_frame: AtomicU64,
    restarts: AtomicU64,
}

impl Shared {
    fn now_nanos(&self) -> u64 {
        self.os.now().as_nanos().max(0) as u64
    }

    fn alive(&self) {
        self.last_frame.store(self.now_nanos(), Ordering::SeqCst);
    }

    fn command(&self, line: &str) {
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipe) = stdin.as_mut()
            && writeln!(pipe, "{line}").and_then(|_| pipe.flush()).is_err()
        {
            *stdin = None;
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleep `delay`, or less if the source is stopped meanwhile. False if
    /// it was.
    fn wait(&self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
        }
        !self.stopped()
    }
}

struct Control(Arc<Shared>);

impl BrowserControl for Control {
    fn refresh(&self) {
        // The hang clock starts again: the page is loading anew.
        self.0.alive();
        self.0.command("reload");
    }

    fn set_loaded(&self, loaded: bool) {
        // Back on air: the hang clock starts now, before `loaded` turns the
        // check on, so time off air (no pictures, by design) never counts.
        if loaded {
            self.0.alive();
        }
        self.0.loaded.store(loaded, Ordering::SeqCst);
        self.0.command(if loaded { "load" } else { "unload" });
    }
}

/// A running browser source; dropping it stops the helper.
pub struct BrowserCapture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl BrowserCapture {
    /// How many times the helper has been restarted.
    pub fn restarts(&self) -> u64 {
        self.shared.restarts.load(Ordering::SeqCst)
    }
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

/// Start rendering `page` in a helper from `os`, delivering pictures drawn
/// into pools from `make_pool` (a new one replaces a pool that failed).
/// Fails, with the helper's advice, if it can't load the page at all.
pub fn start<P, F>(
    os: Arc<dyn HelperOs>,
    timing: Timing,
    page: &BrowserPage<'_>,
    mut make_pool: F,
    sink: VideoSink<P::Frame>,
    audio: Option<AudioSink>,
) -> Result<(BrowserCapture, CaptureFormat, Arc<dyn BrowserControl>)>
where
    P: FramePool + 'static,
    P::Frame: Send + 'static,
    F: FnMut() -> Result<P> + Send + 'static,
{
    let spec = Spec::new(page, audio.is_some());
    let pool = make_pool()?;
    let shared = Arc::new(Shared {
        last_frame: AtomicU64::new(os.now().as_nanos().max(0) as u64),
        os,
        stdin: Mutex::new(None),
        loaded: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        restarts: AtomicU64::new(0),
    });
    // The first start is checked here, so a broken install is an error the
    // caller sees, not a blank source. The source's own thread starts every
    // helper (the engine opens sources on short-lived threads).
    let (ready, started) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let start_timeout = timing.start;
    let thread = std::thread::Builder::new()
        .name("browser-source".into())
        .spawn({
            let shared = shared.clone();
            move || {
                let (process, reader) = match start_helper(&shared, &spec, &timing) {
                    Ok(started) => started,
                    Err(err) => {
                        let _ = ready.send(Err(err));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                let source = Source {
                    shared,
                    spec,
                    timing,
                    sink,
                    audio: audio
                        .map(|sink| (sink, Mutex::new((Stamper::default(), Resampler::default())))),
                };
                source.run(process, reader, pool, make_pool)
            }
        })
        .context("starting the browser source thread")?;
    match started.recv_timeout(start_timeout + Duration::from_secs(5)) {
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
                start_timeout.as_secs()
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

/// Start a helper and wait for its `REDY`; its stdin goes to `shared`.
fn start_helper(shared: &Shared, spec: &Spec, timing: &Timing) -> Result<(Process, Reader)> {
    if shared.stopped() {
        bail!("browser source stopped");
    }
    let (mut process, stdin, stdout) = Process::spawn(&*shared.os, spec)?;
    let mut reader = BufReader::with_capacity(1 << 20, stdout);
    wait_ready(shared, &mut process, &mut reader, timing.start)?;
    *shared.stdin.lock().unwrap_or_else(|e| e.into_inner()) = Some(stdin);
    if !shared.loaded.load(Ordering::SeqCst) {
        shared.command("unload");
    }
    Ok((process, reader))
}

/// Wait for the helper's `REDY` (or its failure), giving up early if the
/// source is stopped.
fn wait_ready(
    shared: &Shared,
    process: &mut Process,
    reader: &mut Reader,
    timeout: Duration,
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
        let deadline = Instant::now() + timeout;
        let result = loop {
            match receive.recv_timeout(POLL) {
                Ok(result) => break Some(result),
                Err(_) if shared.stopped() || Instant::now() >= deadline => break None,
                Err(_) => {}
            }
        };
        if result.is_none() {
            // A hung start or a stopped source: killing it ends the read.
            process.stop();
        }
        result
    });
    match outcome {
        Some(Ok(Ok(()))) => Ok(()),
        Some(Ok(Err(reason))) => {
            let status = process.child.wait().ok().and_then(|s| s.code());
            std::thread::sleep(Duration::from_millis(50)); // let stderr arrive
            let stderr = process
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let reason = (!reason.is_empty()).then_some(reason.as_str());
            Err(anyhow!(
                shared.os.explain_start_failure(status, reason, &stderr)
            ))
        }
        Some(Err(err)) => Err(err),
        None if shared.stopped() => Err(anyhow!("browser source stopped")),
        None => Err(anyhow!(
            "browser source helper didn't start the page within {} s",
            timeout.as_secs()
        )),
    }
}

/// Why reading a helper ended.
enum Ended {
    /// The helper closed its output or sent something wrong.
    Helper(anyhow::Error),
    /// The frame pool failed (a GPU upload): the helper is fine, the pool
    /// isn't.
    Pool(anyhow::Error),
}

struct Source<P: FramePool> {
    shared: Arc<Shared>,
    spec: Spec,
    timing: Timing,
    sink: VideoSink<P::Frame>,
    audio: Option<(AudioSink, Mutex<(Stamper, Resampler)>)>,
}

impl<P: FramePool> Source<P> {
    /// Read pictures and sound until stopped, restarting the helper after a
    /// crash, a hang or too much memory, and replacing a failed pool.
    fn run(
        &self,
        mut process: Process,
        reader: Reader,
        mut pool: P,
        mut make_pool: impl FnMut() -> Result<P>,
    ) {
        let shared = &*self.shared;
        let mut failures = 0usize;
        let mut pool_failures = 0usize;
        let mut pool_since = Instant::now();
        let mut reader = Some(reader);
        loop {
            let input = reader.take().expect("a started helper has a reader");
            shared.alive();
            let reading_ended = AtomicBool::new(false);
            let (why, pool_failed) = std::thread::scope(|scope| {
                let reading = scope.spawn(|| {
                    let result = self.read(input, &mut pool);
                    reading_ended.store(true, Ordering::SeqCst);
                    result
                });
                // The watchdog: stop, hang, memory.
                let mut checked = Instant::now();
                let why = loop {
                    std::thread::sleep(POLL);
                    if reading_ended.load(Ordering::SeqCst) {
                        break None;
                    }
                    if shared.stopped() {
                        break Some("stopped".to_string());
                    }
                    let idle = Duration::from_nanos(
                        shared
                            .now_nanos()
                            .saturating_sub(shared.last_frame.load(Ordering::SeqCst)),
                    );
                    if shared.loaded.load(Ordering::SeqCst) && idle > self.timing.hang {
                        break Some(format!(
                            "no picture for {} s (the page or its browser hung)",
                            idle.as_secs()
                        ));
                    }
                    if checked.elapsed() > Duration::from_secs(2) {
                        checked = Instant::now();
                        let used = process.tree.memory_mb();
                        if used > self.timing.memory_cap_mb {
                            break Some(format!(
                                "it used {used} MB, over the {} MB cap",
                                self.timing.memory_cap_mb
                            ));
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
                match (why, read_result) {
                    (Some(why), _) => (why, None),
                    (None, Ok(Ok(()))) => {
                        (format!("the helper closed its output ({status})"), None)
                    }
                    (None, Ok(Err(Ended::Helper(err)))) => (format!("{err:#} ({status})"), None),
                    (None, Ok(Err(Ended::Pool(err)))) => {
                        (format!("its frames couldn't be stored: {err:#}"), Some(err))
                    }
                    (None, Err(_)) => (format!("reading the helper panicked ({status})"), None),
                }
            });
            if shared.stopped() {
                return;
            }
            let need_pool = pool_failed.is_some();
            if let Some(err) = pool_failed {
                // A broken pool (a failed GPU upload) can't be used again:
                // a new one, a few times, then the source stops with why.
                if pool_since.elapsed() > self.timing.stable {
                    pool_failures = 0;
                }
                pool_failures += 1;
                pool_since = Instant::now();
                if pool_failures >= self.timing.pool_failures {
                    (self.sink.stopped)(format!(
                        "browser source stopped: its frames couldn't be stored {pool_failures} times in a row: {err:#}"
                    ));
                    return;
                }
            }
            if process.started.elapsed() > self.timing.stable {
                failures = 0;
            }
            let delay = self.timing.backoff[failures.min(self.timing.backoff.len() - 1)];
            failures += 1;
            shared.restarts.fetch_add(1, Ordering::SeqCst);
            isocast_diagnostics::diagnostic!("warn";
                "isocastd: browser source restarting its page in {} s: {why}", delay.as_secs()
            );
            if !shared.wait(delay) {
                return;
            }
            if need_pool {
                match make_pool() {
                    Ok(fresh) => pool = fresh,
                    Err(err) => {
                        (self.sink.stopped)(format!(
                            "browser source stopped: no new frame pool: {err:#}"
                        ));
                        return;
                    }
                }
            }
            match start_helper(shared, &self.spec, &self.timing) {
                Ok((started, input)) => {
                    process = started;
                    reader = Some(input);
                }
                Err(_) if shared.stopped() => return,
                Err(err) => {
                    // Not even starting (uninstalled meanwhile?): the source stops.
                    (self.sink.stopped)(format!("{err:#}"));
                    return;
                }
            }
        }
    }

    /// Pictures and sound until the helper's output ends or breaks.
    fn read(&self, mut reader: impl Read, pool: &mut P) -> Result<(), Ended> {
        let shared = &*self.shared;
        let spec = &self.spec;
        let helper = Ended::Helper;
        let mut discard = Vec::new();
        loop {
            let Some(header) = protocol::read_header(&mut reader).map_err(helper)? else {
                return Ok(());
            };
            match header {
                // A beat: the page is loading or still, without new pictures.
                protocol::Header::Ready => shared.alive(),
                protocol::Header::Frame { width, height } => {
                    let len = width as usize * height as usize * 4;
                    if (width, height) != (spec.width, spec.height) {
                        return Err(helper(anyhow!(
                            "browser helper sent {width}x{height} pictures, not {}x{}",
                            spec.width,
                            spec.height
                        )));
                    }
                    let row = width as usize * 4;
                    let mut failed = None;
                    let mut drew = false;
                    let frame = pool
                        .draw(&mut |pixels, stride| {
                            drew = true;
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
                        })
                        .map_err(Ended::Pool)?;
                    if let Some(err) = failed {
                        return Err(helper(
                            anyhow::Error::from(err).context("reading a browser picture"),
                        ));
                    }
                    match frame {
                        Some(frame) => {
                            shared.alive();
                            (self.sink.frame)(frame, shared.os.now());
                        }
                        None => {
                            // Every pooled frame is still downstream: skip it.
                            if !drew {
                                discard.resize(len, 0);
                                reader
                                    .read_exact(&mut discard)
                                    .context("reading a browser picture")
                                    .map_err(helper)?;
                            }
                            shared.alive();
                        }
                    }
                }
                protocol::Header::Audio { rate, len } => {
                    let mut bytes = vec![0u8; len];
                    reader
                        .read_exact(&mut bytes)
                        .context("reading browser audio")
                        .map_err(helper)?;
                    let now = shared.os.now();
                    if let Some((sink, state)) = &self.audio {
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
                    reader
                        .read_exact(&mut reason)
                        .context("reading the browser helper")
                        .map_err(helper)?;
                    return Err(helper(anyhow!("{}", String::from_utf8_lossy(&reason))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
