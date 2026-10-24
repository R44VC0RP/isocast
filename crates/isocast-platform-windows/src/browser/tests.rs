//! Browser sources against fake helpers (Python scripts that speak the
//! helper's protocol), so the process handling, restarts and limits are
//! tested without WebView2 or a GPU. The real helper runs in the e2e
//! scenario `win_browser`.

use std::sync::atomic::AtomicUsize;

use isocast_engine::PageLocation;
use windows::Win32::Foundation::STILL_ACTIVE;
use windows::Win32::System::Threading::GetExitCodeProcess;

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
state = os.path.join(here, "state")
starts = int(open(state).read()) if os.path.exists(state) else 0
open(state, "w").write(str(starts + 1))
"#;

struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str, body: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "isocast-browser-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("helper.py"), format!("{PRELUDE}{body}")).unwrap();
        Self { dir }
    }

    fn helper(&self) -> Helper {
        Helper {
            program: std::env::var_os("PYTHON").map_or_else(|| "python".into(), PathBuf::from),
            prefix: vec![
                "-u".into(),
                self.dir.join("helper.py").to_string_lossy().into_owned(),
            ],
        }
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
        // The helper's job has ended; its files may take a moment to close.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// CPU frames: the BGRA bytes drawn.
struct Pool {
    width: u32,
    height: u32,
    out: Arc<AtomicUsize>,
}

impl FramePool for Pool {
    type Frame = Vec<u8>;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Vec<u8>>> {
        // At most four frames downstream, like a real pool running dry.
        if self.out.load(Ordering::SeqCst) >= 4 {
            return Ok(None);
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
}

fn sinks(seen: &Arc<Seen>) -> (Pool, VideoSink<Vec<u8>>, AudioSink) {
    let (a, b, c, d) = (seen.clone(), seen.clone(), seen.clone(), seen.clone());
    let page = page();
    (
        Pool {
            width: page.width,
            height: page.height,
            out: Arc::new(AtomicUsize::new(0)),
        },
        VideoSink {
            frame: Box::new(move |frame: Vec<u8>, _| {
                a.frames.fetch_add(1, Ordering::SeqCst);
                // Every byte of a picture is its value: rows were placed
                // whole, whatever the pool's stride.
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

fn wait_for(what: &str, seconds: u64, ok: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let mut code = 0u32;
    let running =
        unsafe { GetExitCodeProcess(process, &mut code) }.is_ok() && code == STILL_ACTIVE.0 as u32;
    let _ = unsafe { CloseHandle(process) };
    running
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
fn a_missing_runtime_names_where_to_get_it() {
    let text = explain_start_failure(
        Some(3),
        Some(
            "browser sources need the Microsoft Edge WebView2 Runtime, which isn't installed: \
             get it from https://go.microsoft.com/fwlink/p/?LinkId=2124703",
        ),
        "",
    );
    assert!(text.contains("go.microsoft.com/fwlink"), "{text}");
    let text = explain_start_failure(Some(0xC000_0135_u32 as i32), None, "");
    assert!(text.contains("reinstall Isocast"), "{text}");
}

#[test]
fn memory_of_a_job_counts_its_processes() {
    let job = Job::new().unwrap();
    let mut child =
        Command::new(std::env::var_os("PYTHON").map_or_else(|| "python".into(), PathBuf::from))
            .args(["-c", "import time; time.sleep(10)"])
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .unwrap();
    assert_eq!(job.memory_mb(), 0, "an empty job");
    unsafe { AssignProcessToJobObject(*job.0, HANDLE(child.as_raw_handle())) }.unwrap();
    wait_for("the child's working set", 10, || job.memory_mb() > 0);
    job.kill();
    let _ = child.wait();
}

#[test]
fn pictures_audio_and_commands_flow_through_the_helper() {
    let fake = Fake::new(
        "flow",
        r#"
rec(b"REDY")
open(os.path.join(here, "argv"), "w").write(" ".join(args))
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
    let (pool, video, audio) = sinks(&seen);
    let (capture, format, control) =
        start(fake.helper(), &page(), pool, video, Some(audio)).unwrap();
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
    // The page went on stdin, never in the (readable) command line.
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
    os._exit(0xC0000005 & 0x7fffffff)  # as WebView2 crashing would
for n in range(1000):
    frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (pool, video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.helper(), &page(), pool, video, None).unwrap();
    wait_for("pictures after the restart", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(fake.starts(), 2);
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 1);
    assert!(
        seen.stopped.lock().unwrap().is_none(),
        "a crash restarts, it doesn't stop the source"
    );
}

#[test]
fn a_page_failure_restarts_the_helper() {
    // The helper ends with FAIL when WebView2's renderer dies or hangs.
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
    let (pool, video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.helper(), &page(), pool, video, None).unwrap();
    wait_for("pictures after the restart", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 1);
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
    let (pool, video, _) = sinks(&seen);
    let started = Instant::now();
    let (_capture, _, _) = start(fake.helper(), &page(), pool, video, None).unwrap();
    wait_for("pictures after the hang", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert!(
        started.elapsed() >= HANG_TIMEOUT,
        "it waited for the hang timeout"
    );
    assert_eq!(fake.starts(), 2);
}

#[test]
fn a_still_page_that_beats_is_not_a_hang() {
    // WebView2's capture only makes pictures when the page changes.
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
    let (pool, video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.helper(), &page(), pool, video, None).unwrap();
    std::thread::sleep(HANG_TIMEOUT * 3);
    assert_eq!(fake.starts(), 1, "no restart for a still page");
    assert_eq!(seen.frames.load(Ordering::SeqCst), 1);
    drop(capture);
}

#[test]
fn an_unloaded_page_is_not_a_hang() {
    let fake = Fake::new(
        "unloaded",
        r#"
rec(b"REDY")
frame(w, h, 1)
time.sleep(3600)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (pool, video, _) = sinks(&seen);
    let (capture, _, control) = start(fake.helper(), &page(), pool, video, None).unwrap();
    control.set_loaded(false);
    std::thread::sleep(HANG_TIMEOUT * 2);
    assert_eq!(fake.starts(), 1, "no restart while the page is unloaded");
    drop(capture);
}

#[test]
fn a_helper_that_cannot_start_fails_the_source_with_the_reason() {
    let fake = Fake::new(
        "fail",
        r#"
rec(b"FAIL", b"browser sources need the Microsoft Edge WebView2 Runtime, which isn't installed: get it from https://go.microsoft.com/fwlink/p/?LinkId=2124703")
sys.exit(3)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (pool, video, _) = sinks(&seen);
    let err = start(fake.helper(), &page(), pool, video, None)
        .err()
        .expect("it fails")
        .to_string();
    assert!(
        err.contains("WebView2 Runtime") && err.contains("LinkId=2124703"),
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
    let (pool, video, _) = sinks(&seen);
    let err = start(silent.helper(), &page(), pool, video, None)
        .err()
        .expect("it fails")
        .to_string();
    assert!(
        err.contains("status 0x7") && err.contains("something went wrong"),
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
    let (pool, video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.helper(), &page(), pool, video, None).unwrap();
    wait_for("the restart", 10, || {
        capture.shared.restarts.load(Ordering::SeqCst) >= 1
    });
    assert_eq!(seen.frames.load(Ordering::SeqCst), 0);
}

#[test]
fn the_helpers_own_processes_end_with_the_source() {
    // WebView2's browser processes are the helper's children: they share
    // its job and end with it, even if the helper never cleans up.
    let fake = Fake::new(
        "children",
        r#"
import subprocess
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(3600)"])
open(os.path.join(here, "child"), "w").write(str(child.pid))
rec(b"REDY")
for n in range(100000):
    frame(w, h, 1); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (pool, video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.helper(), &page(), pool, video, None).unwrap();
    wait_for("the child", 10, || !fake.file("child").is_empty());
    let pid: u32 = fake.file("child").trim().parse().unwrap();
    assert!(alive(pid), "the helper's child runs");
    drop(capture);
    wait_for("the child to end", 10, || !alive(pid));
}
