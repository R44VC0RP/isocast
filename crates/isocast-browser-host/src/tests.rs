//! The host against fake helpers (Python scripts that speak the helper's
//! protocol), so process handling, restarts and limits are tested without
//! a browser engine or a GPU, on every OS CI runs. The real helpers run in
//! the e2e scenarios linux_browser and win_browser.

use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use super::*;

/// A fake helper: Python with `struct`, writing records to stdout.
const PRELUDE: &str = r#"
import os, struct, sys, time
out = sys.stdout.buffer
def rec(tag, payload=b""):
    out.write(tag + struct.pack("<I", len(payload)) + payload); out.flush()
def frame(w, h, value):
    rec(b"FRAM", struct.pack("<II", w, h) + bytes([value]) * (w * h * 4))
args = sys.argv[1:]
w = int(args[args.index("--width") + 1]); h = int(args[args.index("--height") + 1])
here = os.path.dirname(os.path.abspath(__file__))
page = sys.stdin.readline()
open(os.path.join(here, "page"), "w").write(page)
open(os.path.join(here, "argv"), "w").write(" ".join(args))
state = os.path.join(here, "state")
starts = int(open(state).read()) if os.path.exists(state) else 0
open(state, "w").write(str(starts + 1))
"#;

fn python() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".into()
        } else {
            "python3".into()
        }
    })
}

struct Fake {
    dir: std::path::PathBuf,
    /// What the fake's process tree "uses", in MiB.
    memory: Arc<AtomicU64>,
}

impl Fake {
    fn new(name: &str, body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "isocast-browser-host-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("helper.py"), format!("{PRELUDE}{body}")).unwrap();
        Self {
            dir,
            memory: Arc::new(AtomicU64::new(1)),
        }
    }

    fn os(&self) -> Arc<dyn HelperOs> {
        Arc::new(FakeOs {
            script: self.dir.join("helper.py"),
            memory: self.memory.clone(),
            epoch: Instant::now(),
        })
    }

    fn starts(&self) -> usize {
        std::fs::read_to_string(self.dir.join("state")).map_or(0, |s| s.trim().parse().unwrap_or(0))
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

struct FakeOs {
    script: std::path::PathBuf,
    memory: Arc<AtomicU64>,
    epoch: Instant,
}

struct FakeTree(Arc<AtomicU64>);

impl Confinement for FakeTree {
    fn kill(&self) {}
    fn memory_mb(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl HelperOs for FakeOs {
    fn command(&self) -> Command {
        let mut command = Command::new(python());
        command.arg("-u").arg(&self.script);
        command
    }
    fn adopt(&self, _child: &Child) -> Result<Box<dyn Confinement>> {
        Ok(Box::new(FakeTree(self.memory.clone())))
    }
    fn now(&self) -> Time {
        Time::from_nanos(self.epoch.elapsed().as_nanos() as i64 + 1_000_000_000)
    }
    fn explain_start_failure(
        &self,
        status: Option<i32>,
        reason: Option<&str>,
        stderr: &str,
    ) -> String {
        format!(
            "status {status:?}, reason {reason:?}, stderr {:?}",
            stderr.trim()
        )
    }
}

fn timing() -> Timing {
    Timing {
        start: Duration::from_secs(10),
        hang: Duration::from_millis(800),
        backoff: vec![Duration::from_millis(100)],
        stable: Duration::from_secs(60),
        memory_cap_mb: 100,
        pool_failures: 3,
    }
}

/// CPU frames: the BGRA bytes drawn. `fail` makes every draw an error, as
/// a pool whose GPU upload broke.
struct Pool {
    width: u32,
    height: u32,
    fail: bool,
}

impl FramePool for Pool {
    type Frame = Vec<u8>;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Vec<u8>>> {
        if self.fail {
            bail!("output pool is unusable after a failed upload");
        }
        let stride = self.width as usize * 4 + 16;
        let mut pixels = vec![0u8; stride * self.height as usize];
        draw(&mut pixels, stride);
        Ok(Some(
            pixels
                .chunks(stride)
                .flat_map(|row| row[..self.width as usize * 4].to_vec())
                .collect(),
        ))
    }

    fn take(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[derive(Default)]
struct Seen {
    frames: AtomicUsize,
    first_byte: Mutex<Vec<u8>>,
    whole: Mutex<bool>,
    audio_frames: AtomicUsize,
    stopped: Mutex<Option<String>>,
    pools: AtomicUsize,
}

fn sinks(seen: &Arc<Seen>) -> (VideoSink<Vec<u8>>, AudioSink) {
    let (a, b, c, d) = (seen.clone(), seen.clone(), seen.clone(), seen.clone());
    (
        VideoSink {
            frame: Box::new(move |frame: Vec<u8>, _| {
                a.frames.fetch_add(1, Ordering::SeqCst);
                // Every byte is the picture's value: rows were placed whole,
                // whatever the pool's stride.
                *a.whole.lock().unwrap() = frame.iter().all(|b| *b == frame[0]);
                a.first_byte.lock().unwrap().push(frame[0]);
            }),
            stopped: Box::new(move |why| *b.stopped.lock().unwrap() = Some(why)),
        },
        AudioSink {
            samples: Box::new(move |samples, _| {
                c.audio_frames
                    .fetch_add(samples.len() / 2, Ordering::SeqCst);
            }),
            stopped: Box::new(move |why| *d.stopped.lock().unwrap() = Some(why)),
        },
    )
}

fn page() -> BrowserPage<'static> {
    BrowserPage {
        location: PageLocation::Url("http://example.invalid/"),
        width: 8,
        height: 4,
        fps: 30,
        css: "",
    }
}

fn pools(seen: &Arc<Seen>, fail: bool) -> impl FnMut() -> Result<Pool> + Send + 'static {
    let seen = seen.clone();
    move || {
        seen.pools.fetch_add(1, Ordering::SeqCst);
        Ok(Pool {
            width: 8,
            height: 4,
            fail,
        })
    }
}

type Started = (BrowserCapture, CaptureFormat, Arc<dyn BrowserControl>);

fn start_fake(fake: &Fake, seen: &Arc<Seen>, audio: bool) -> Result<Started> {
    let (video, sound) = sinks(seen);
    start(
        fake.os(),
        timing(),
        &page(),
        pools(seen, false),
        video,
        audio.then_some(sound),
    )
}

fn wait_for(what: &str, seconds: u64, ok: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn audio_is_stamped_continuously_and_resampled() {
    let mut stamper = Stamper::default();
    let start = Time::from_nanos(1_000_000_000);
    let first = stamper.stamp(480, start);
    // The next chunk arrives a little late: it still continues the first.
    let second = stamper.stamp(480, start + Duration::from_millis(12));
    assert_eq!(second, first.add_ticks(480, SAMPLE_RATE));
    // A gap over the resync limit starts afresh from the host clock.
    let third = stamper.stamp(480, start + Duration::from_secs(2));
    assert_eq!(
        third,
        start + Duration::from_secs(2) - Duration::from_millis(10)
    );

    let mut resampler = Resampler::default();
    let input: Vec<f32> = (0..44100).flat_map(|i| [i as f32, -(i as f32)]).collect();
    let output = resampler.run(44100, input);
    let frames = output.len() / 2;
    assert!(
        (47_990..=48_000).contains(&frames),
        "{frames} frames from a second at 44.1 kHz"
    );
    assert_eq!(resampler.run(SAMPLE_RATE, vec![1.0, 2.0]), vec![1.0, 2.0]);
}

#[test]
fn pictures_audio_and_commands_flow_through_the_helper() {
    let fake = Fake::new(
        "flow",
        r#"
rec(b"REDY")
import threading
def commands():
    for line in sys.stdin:
        open(os.path.join(here, "commands"), "a").write(line)
threading.Thread(target=commands, daemon=True).start()
rec(b"AUDI", struct.pack("<I", 48000) + struct.pack("<2f", 0.1, 0.2) * 480)
for n in range(1000):
    frame(w, h, n % 200); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, format, control) = start_fake(&fake, &seen, true).unwrap();
    assert_eq!((format.width, format.height, format.fps), (8, 4, 30));
    wait_for("pictures", 10, || seen.frames.load(Ordering::SeqCst) >= 10);
    wait_for("sound", 10, || {
        seen.audio_frames.load(Ordering::SeqCst) == 480
    });
    let firsts = seen.first_byte.lock().unwrap().clone();
    assert!(
        firsts.windows(2).all(|w| w[1] == w[0] + 1),
        "pictures in order: {firsts:?}"
    );
    assert!(
        *seen.whole.lock().unwrap(),
        "rows placed at the pool's stride"
    );
    control.refresh();
    control.set_loaded(false);
    control.set_loaded(true);
    wait_for("commands", 10, || {
        fake.file("commands").lines().count() == 3
    });
    assert_eq!(
        fake.file("commands").lines().collect::<Vec<_>>(),
        ["reload", "unload", "load"]
    );
    drop(capture);
    assert_eq!(fake.starts(), 1);
    // The page went on stdin, never in the command line.
    assert_eq!(
        protocol::parse_page_line(&fake.file("page")).unwrap(),
        (
            protocol::Location::Url("http://example.invalid/".into()),
            String::new()
        )
    );
    assert!(
        !fake.file("argv").contains("example"),
        "{}",
        fake.file("argv")
    );
}

#[test]
fn a_crashed_helper_is_restarted_and_pictures_resume() {
    let fake = Fake::new(
        "crash",
        r#"
rec(b"REDY")
for n in range(5):
    frame(w, h, 1); time.sleep(1 / 30)
if starts == 0:
    os._exit(9)  # as the browser engine crashing would
for n in range(1000):
    frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    wait_for("pictures after the restart", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(fake.starts(), 2);
    assert_eq!(capture.restarts(), 1);
    assert!(
        seen.stopped.lock().unwrap().is_none(),
        "a crash restarts, it doesn't stop the source"
    );
}

#[test]
fn a_page_failure_restarts_the_helper() {
    let fake = Fake::new(
        "fail-later",
        r#"
rec(b"REDY")
frame(w, h, 1)
if starts == 0:
    rec(b"FAIL", b"the page's renderer exited"); sys.exit(4)
for n in range(1000):
    frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    wait_for("pictures after the restart", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(capture.restarts(), 1);
}

#[test]
fn a_hung_helper_is_killed_and_restarted() {
    let fake = Fake::new(
        "hang",
        r#"
rec(b"REDY")
frame(w, h, 1)
if starts == 0:
    time.sleep(3600)  # alive, but no more pictures
for n in range(1000):
    frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let started = Instant::now();
    let (_capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    wait_for("pictures after the hang", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert!(
        started.elapsed() >= timing().hang,
        "it waited for the hang timeout"
    );
    assert_eq!(fake.starts(), 2);
}

#[test]
fn a_still_page_that_beats_is_not_a_hang() {
    // A capture that only makes pictures when the page changes.
    let fake = Fake::new(
        "still",
        r#"
rec(b"REDY")
frame(w, h, 1)
for n in range(10000):
    rec(b"REDY"); time.sleep(0.2)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    std::thread::sleep(timing().hang * 3);
    assert_eq!(fake.starts(), 1, "no restart for a still page");
    assert_eq!(seen.frames.load(Ordering::SeqCst), 1);
    drop(capture);
}

#[test]
fn an_unloaded_page_is_not_a_hang_nor_when_it_comes_back() {
    // The helper sends nothing while unloaded, and on `load` its page needs
    // a moment before the first picture: neither is a hang.
    let fake = Fake::new(
        "unloaded",
        r#"
rec(b"REDY")
frame(w, h, 1)
for line in sys.stdin:
    if line.strip() == "load":
        time.sleep(0.3)  # past the watchdog's poll, within the hang timeout
        for n in range(1000):
            frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, control) = start_fake(&fake, &seen, false).unwrap();
    wait_for("the first picture", 10, || {
        seen.frames.load(Ordering::SeqCst) >= 1
    });
    control.set_loaded(false);
    std::thread::sleep(timing().hang * 2);
    assert_eq!(fake.starts(), 1, "no restart while the page is unloaded");
    control.set_loaded(true);
    wait_for("pictures after loading again", 10, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(
        fake.starts(),
        1,
        "loading again after a long unload isn't a hang"
    );
    assert_eq!(capture.restarts(), 0);
}

#[test]
fn a_helper_that_cannot_start_fails_the_source_with_its_reason() {
    let fake = Fake::new(
        "fail",
        r#"
rec(b"FAIL", b"the browser runtime isn't installed: get it from https://example.invalid/runtime")
sys.exit(3)
"#,
    );
    let seen = Arc::new(Seen::default());
    let err = start_fake(&fake, &seen, false)
        .err()
        .expect("it fails")
        .to_string();
    assert!(
        err.contains("example.invalid/runtime") && err.contains("Some(3)"),
        "{err}"
    );

    let silent = Fake::new(
        "silent",
        r#"
sys.stderr.write("something went wrong\n")
sys.stderr.flush()
sys.exit(7)
"#,
    );
    let err = start_fake(&silent, &seen, false)
        .err()
        .expect("it fails")
        .to_string();
    assert!(
        err.contains("Some(7)") && err.contains("something went wrong"),
        "{err}"
    );
}

#[test]
fn pictures_of_the_wrong_size_are_an_error_not_a_frame() {
    let fake = Fake::new(
        "size",
        r#"
rec(b"REDY")
frame(w + 1, h, 1)
time.sleep(3600)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    wait_for("the restart", 10, || capture.restarts() >= 1);
    assert_eq!(seen.frames.load(Ordering::SeqCst), 0);
}

#[test]
fn a_helper_over_the_memory_cap_is_restarted() {
    let fake = Fake::new(
        "memory",
        r#"
rec(b"REDY")
for n in range(100000):
    frame(w, h, 1); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    fake.memory
        .store(timing().memory_cap_mb + 1, Ordering::SeqCst);
    wait_for("the restart", 10, || capture.restarts() >= 1);
    fake.memory.store(1, Ordering::SeqCst);
    assert!(seen.stopped.lock().unwrap().is_none());
}

#[test]
fn a_frame_pool_that_keeps_failing_stops_the_source_with_why() {
    // A failed GPU upload leaves a pool unusable: the source gets new pools
    // (restarting the helper, whose stream broke mid-picture) a few times,
    // then stops with the reason instead of restarting forever.
    let fake = Fake::new(
        "pool",
        r#"
rec(b"REDY")
for n in range(100000):
    frame(w, h, 1); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(
        fake.os(),
        timing(),
        &page(),
        pools(&seen, true),
        video,
        None,
    )
    .unwrap();
    wait_for("the source to stop", 15, || {
        seen.stopped.lock().unwrap().is_some()
    });
    let why = seen.stopped.lock().unwrap().clone().unwrap();
    assert!(
        why.contains("couldn't be stored 3 times") && why.contains("failed upload"),
        "{why}"
    );
    assert_eq!(
        seen.pools.load(Ordering::SeqCst),
        3,
        "the first pool and two new ones"
    );
    assert_eq!(capture.restarts(), 2);
    assert_eq!(seen.frames.load(Ordering::SeqCst), 0);
}

#[test]
fn stopping_during_a_slow_restart_returns_at_once() {
    // A restarted helper that never says it's loading would hold the source
    // thread for the whole start timeout; dropping the source must not.
    let fake = Fake::new(
        "slow-restart",
        r#"
if starts > 0:
    time.sleep(3600)
rec(b"REDY")
frame(w, h, 1)
os._exit(1)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, _) = start_fake(&fake, &seen, false).unwrap();
    wait_for("the second start", 10, || fake.starts() >= 2);
    let dropped = Instant::now();
    drop(capture);
    assert!(
        dropped.elapsed() < Duration::from_secs(2),
        "drop took {:?}",
        dropped.elapsed()
    );
}

#[test]
fn a_refreshed_page_starts_the_hang_clock_again() {
    // A still page (no beats) is refreshed just before the hang timeout,
    // and its reloaded page answers 300 ms later: loading anew, not a hang.
    let fake = Fake::new(
        "refresh",
        r#"
rec(b"REDY")
frame(w, h, 1)
for line in sys.stdin:
    if line.strip() == "reload":
        time.sleep(0.3)
        for n in range(1000):
            frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (capture, _, control) = start_fake(&fake, &seen, false).unwrap();
    wait_for("the first picture", 10, || {
        seen.frames.load(Ordering::SeqCst) >= 1
    });
    std::thread::sleep(timing().hang * 3 / 4);
    control.refresh();
    wait_for("pictures after the reload", 10, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(capture.restarts(), 0, "a refresh isn't a hang");
    assert_eq!(fake.starts(), 1);
}
