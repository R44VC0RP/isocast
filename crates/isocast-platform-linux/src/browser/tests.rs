//! Browser sources against fake helpers (Python scripts that speak the
//! helper's protocol), so the process handling, restarts and limits are
//! tested without WebKit. The real helper runs in the e2e scenario.

use std::sync::atomic::AtomicUsize;

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
state = os.environ["FAKE_STATE"]
here = os.path.dirname(state)
open(os.path.join(here, "page"), "w").write(sys.stdin.readline())
open(os.path.join(here, "argv"), "w").write(" ".join(args))
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
        let script = dir.join("helper");
        let state = dir.join("state");
        std::fs::write(
            &script,
            format!(
                "#!/usr/bin/env -S FAKE_STATE={} python3\n{PRELUDE}{body}",
                state.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("helper")
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
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Default)]
struct Seen {
    frames: AtomicUsize,
    first_byte: Mutex<Vec<u8>>,
    audio_frames: AtomicUsize,
    stopped: Mutex<Option<String>>,
}

fn sinks(seen: &Arc<Seen>) -> (VideoSink<Frame>, AudioSink) {
    let (a, b, c, d) = (seen.clone(), seen.clone(), seen.clone(), seen.clone());
    (
        VideoSink {
            frame: Box::new(move |frame: Frame, _| {
                a.frames.fetch_add(1, Ordering::SeqCst);
                a.first_byte
                    .lock()
                    .unwrap()
                    .push(frame.read_bgra().unwrap()[0]);
            }),
            stopped: Box::new(move |why| *b.stopped.lock().unwrap() = Some(why)),
        },
        AudioSink {
            report_gap: Box::new(|_, gap| panic!("unexpected browser gap: {gap:?}")),
            samples: Box::new(move |samples, _| {
                c.audio_frames
                    .fetch_add(samples.len() / 2, Ordering::SeqCst);
            }),
            stopped: Box::new(move |why| *d.stopped.lock().unwrap() = Some(why)),
        },
    )
}

fn unhex(word: &str) -> String {
    let bytes: Vec<u8> = (0..word.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&word[i..i + 2], 16).unwrap())
        .collect();
    String::from_utf8(bytes).unwrap()
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

#[test]
fn protocol_reads_each_record_and_rejects_bad_ones() {
    let mut bytes = Vec::new();
    bytes.extend(b"REDY\0\0\0\0");
    bytes.extend(b"FRAM");
    bytes.extend(&(8u32 + 2 * 4).to_le_bytes());
    bytes.extend(&2u32.to_le_bytes());
    bytes.extend(&1u32.to_le_bytes());
    let pixels_at = bytes.len();
    bytes.extend([7u8; 8]);
    bytes.extend(b"AUDI");
    bytes.extend(&(4u32 + 8).to_le_bytes());
    bytes.extend(&44100u32.to_le_bytes());
    bytes.extend([0u8; 8]);
    let mut input = &bytes[..];
    assert_eq!(
        protocol::read_header(&mut input).unwrap(),
        Some(protocol::Header::Ready)
    );
    assert_eq!(
        protocol::read_header(&mut input).unwrap(),
        Some(protocol::Header::Frame {
            width: 2,
            height: 1
        })
    );
    assert_eq!(bytes.len() - input.len(), pixels_at);
    input = &input[8..];
    assert_eq!(
        protocol::read_header(&mut input).unwrap(),
        Some(protocol::Header::Audio {
            rate: 44100,
            len: 8
        })
    );
    input = &input[8..];
    assert_eq!(protocol::read_header(&mut input).unwrap(), None);

    // A picture whose size doesn't match its length, an oversized record,
    // and an unknown tag are errors, not guesses.
    let mut bad = b"FRAM".to_vec();
    bad.extend(&100u32.to_le_bytes());
    bad.extend(&2u32.to_le_bytes());
    bad.extend(&1u32.to_le_bytes());
    assert!(protocol::read_header(&mut &bad[..]).is_err());
    let mut huge = b"FRAM".to_vec();
    huge.extend(&u32::MAX.to_le_bytes());
    assert!(protocol::read_header(&mut &huge[..]).is_err());
    assert!(protocol::read_header(&mut &b"WHAT\0\0\0\0"[..]).is_err());
}

fn audio_record(rate: u32, bytes: usize) -> Vec<u8> {
    let mut record = b"AUDI".to_vec();
    record.extend(&(4 + bytes as u32).to_le_bytes());
    record.extend(&rate.to_le_bytes());
    record.extend(vec![0u8; bytes]);
    record
}

#[test]
fn protocol_refuses_audio_rates_and_sizes_a_page_could_abuse() {
    // A page picks the rate (audio.js): resampling 1 Hz to 48 kHz would
    // ask for 48,000 times the input. Only 8–192 kHz pass.
    for rate in [0, 1, 7_999, 192_001, u32::MAX] {
        let err = protocol::read_header(&mut &audio_record(rate, 8)[..]).unwrap_err();
        assert!(err.to_string().contains("Hz"), "{rate}: {err:#}");
    }
    for rate in [8_000, 44_100, 48_000, 192_000] {
        assert_eq!(
            protocol::read_header(&mut &audio_record(rate, 8)[..]).unwrap(),
            Some(protocol::Header::Audio { rate, len: 8 })
        );
    }
    // Audio and failure reasons have small caps, not the picture's.
    let mut huge_audio = b"AUDI".to_vec();
    huge_audio.extend(&(4 + protocol::MAX_AUDIO + 8).to_le_bytes());
    assert!(protocol::read_header(&mut &huge_audio[..]).is_err());
    let mut huge_fail = b"FAIL".to_vec();
    huge_fail.extend(&(protocol::MAX_FAIL + 1).to_le_bytes());
    assert!(protocol::read_header(&mut &huge_fail[..]).is_err());
    assert!(protocol::read_header(&mut &b"REDY\x01\0\0\0"[..]).is_err());
    // And the resampler never sizes a buffer from a refused rate.
    assert!(Resampler::default().run(1, vec![0.0; 960]).is_empty());
}

#[test]
fn a_page_claiming_a_tiny_audio_rate_restarts_its_helper_not_isocastd() {
    let fake = Fake::new(
        "rate",
        r#"
rec(b"REDY")
frame(w, h, 1)
if starts == 0:
    rec(b"AUDI", struct.pack("<I", 1) + struct.pack("<2f", 0.1, 0.2) * 48000)
for n in range(1000):
    frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, audio) = sinks(&seen);
    let (capture, _, _) = start(fake.path(), &page(), video, Some(audio)).unwrap();
    wait_for("pictures after the restart", 15, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 1);
    assert_eq!(
        seen.audio_frames.load(Ordering::SeqCst),
        0,
        "the bad sound never reached the mixer"
    );
}

#[test]
fn audio_is_stamped_continuously_and_resampled() {
    let mut stamper = Stamper::default();
    let start = isocast_engine::Time::from_nanos(1_000_000_000);
    let first = stamper.stamp(480, start);
    // The next chunk arrives a little late: it still continues the first.
    let second = stamper.stamp(480, start + Duration::from_millis(12));
    assert_eq!(second, (first.0.add_ticks(480, SAMPLE_RATE), None));
    // A gap over the resync limit starts afresh from the host clock.
    let third = stamper.stamp(480, start + Duration::from_secs(2));
    assert_eq!(
        third,
        (
            start + Duration::from_secs(2) - Duration::from_millis(10),
            None
        )
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
fn a_missing_library_names_the_packages() {
    let stderr = "/opt/isocast/isocast-browser: error while loading shared libraries: \
                  libwebkitgtk-6.0.so.4: cannot open shared object file: No such file or directory\n";
    let text = explain_start_failure(Some(127), None, stderr);
    assert!(text.contains("libwebkitgtk-6.0.so.4"), "{text}");
    assert!(
        text.contains("libwebkitgtk-6.0-4") && text.contains("webkitgtk6.0"),
        "{text}"
    );
    let text = explain_start_failure(
        Some(3),
        Some("gtk4-broadwayd is not installed: install libgtk-4-bin"),
        "",
    );
    assert!(text.contains("libgtk-4-bin"), "{text}");
}

#[test]
fn memory_of_a_process_tree_counts_its_children() {
    let mut child = Command::new("sleep").arg("5").spawn().unwrap();
    let mine = tree_memory_mb(std::process::id());
    assert!(mine > 0, "this test's own memory");
    let alone = tree_memory_mb(child.id());
    assert!(mine >= alone);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn pictures_audio_and_commands_flow_through_the_helper() {
    let fake = Fake::new(
        "flow",
        r#"
rec(b"REDY")
import select, threading
def commands():
    for line in sys.stdin:
        open(os.path.join(os.path.dirname(state), "commands"), "a").write(line)
threading.Thread(target=commands, daemon=True).start()
rec(b"AUDI", struct.pack("<I", 48000) + struct.pack("<2f", 0.1, 0.2) * 480)
for n in range(1000):
    frame(w, h, n % 200); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, audio) = sinks(&seen);
    let (capture, format, control) = start(fake.path(), &page(), video, Some(audio)).unwrap();
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
    control.refresh();
    control.set_loaded(false);
    control.set_loaded(true);
    wait_for("commands", 10, || {
        fake.file("commands").lines().count() == 3
    });
    assert_eq!(fake.file("commands"), "reload\nunload\nload\n");
    // The page went over stdin, not in the world-readable arguments.
    let line = fake.file("page");
    let words: Vec<&str> = line.trim_end().split(' ').collect();
    assert_eq!(words[..2], ["page", "url"], "{line:?}");
    assert_eq!(unhex(words[2]), "http://example.invalid/");
    assert!(
        !fake.file("argv").contains("example.invalid"),
        "{}",
        fake.file("argv")
    );
    drop(capture);
    assert_eq!(fake.starts(), 1);
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
    os.kill(os.getpid(), 11)  # SIGSEGV, as WebKit crashing would
for n in range(1000):
    frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.path(), &page(), video, None).unwrap();
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
    let (video, _) = sinks(&seen);
    let started = Instant::now();
    let (_capture, _, _) = start(fake.path(), &page(), video, None).unwrap();
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
fn an_unloaded_page_is_not_a_hang() {
    // Like the real helper: pictures stop on `unload`, and resume after
    // `load` once the page has loaded again (300 ms here: past the
    // watchdog's 100 ms poll, within the hang timeout).
    let fake = Fake::new(
        "unloaded",
        r#"
rec(b"REDY")
frame(w, h, 1)
for line in sys.stdin:
    if line.strip() == "load":
        time.sleep(0.3)
        for n in range(100000):
            frame(w, h, 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, control) = start(fake.path(), &page(), video, None).unwrap();
    control.set_loaded(false);
    std::thread::sleep(HANG_TIMEOUT * 2);
    assert_eq!(fake.starts(), 1, "no restart while the page is unloaded");
    // Back on air after longer than the hang timeout off air: not a hang.
    control.set_loaded(true);
    wait_for("pictures back on air", 10, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        fake.starts(),
        1,
        "going back on air didn't restart the helper"
    );
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 0);
    drop(capture);
}

#[test]
fn a_helper_that_cannot_start_fails_the_source_with_the_reason() {
    let fake = Fake::new(
        "fail",
        r#"
rec(b"FAIL", b"gtk4-broadwayd is not installed: install libgtk-4-bin (Debian/Ubuntu)")
sys.exit(3)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let err = start(fake.path(), &page(), video, None)
        .err()
        .expect("it fails")
        .to_string();
    assert!(err.contains("libgtk-4-bin"), "{err}");

    let missing = Fake::new(
        "missing",
        r#"
sys.stderr.write("isocast-browser: error while loading shared libraries: libwebkitgtk-6.0.so.4: cannot open shared object file\n")
sys.stderr.flush()
sys.exit(127)
"#,
    );
    let (video, _) = sinks(&seen);
    let err = start(missing.path(), &page(), video, None)
        .err()
        .expect("it fails")
        .to_string();
    assert!(err.contains("libwebkitgtk-6.0-4"), "{err}");
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
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.path(), &page(), video, None).unwrap();
    wait_for("the restart", 10, || {
        capture.shared.restarts.load(Ordering::SeqCst) >= 1
    });
    assert_eq!(seen.frames.load(Ordering::SeqCst), 0);
}

#[test]
fn a_source_opened_on_a_short_lived_thread_keeps_its_helper() {
    // The engine opens sources on threads that end right after; the helper
    // must outlive them (PR_SET_PDEATHSIG follows the spawning thread).
    let fake = Fake::new(
        "thread",
        r#"
rec(b"REDY")
for n in range(1000):
    frame(w, h, n % 200); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let path = fake.path();
    let capture = std::thread::spawn(move || start(path, &page(), video, None).map(|(c, _, _)| c))
        .join()
        .unwrap()
        .unwrap();
    let before = seen.frames.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        seen.frames.load(Ordering::SeqCst) > before + 20,
        "pictures keep coming"
    );
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.starts(), 1);
}

#[test]
fn a_page_still_loading_is_not_a_hang() {
    // Beats for over twice the hang timeout, then pictures.
    let fake = Fake::new(
        "loading",
        r#"
rec(b"REDY")
for n in range(10):
    time.sleep(0.25); rec(b"REDY")
for n in range(1000):
    frame(w, h, 1); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.path(), &page(), video, None).unwrap();
    wait_for("pictures once loaded", 10, || {
        seen.frames.load(Ordering::SeqCst) > 0
    });
    assert!(HANG_TIMEOUT * 2 < Duration::from_millis(2500));
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 0);
    assert_eq!(fake.starts(), 1);
}

#[test]
fn a_helper_that_fails_and_lingers_is_not_waited_on_forever() {
    let fake = Fake::new(
        "linger",
        r#"
rec(b"FAIL", b"GTK could not start on its private display")
time.sleep(3600)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let started = Instant::now();
    let err = start(fake.path(), &page(), video, None)
        .err()
        .expect("it fails")
        .to_string();
    assert!(err.contains("GTK could not start"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_stopped_source_lets_its_helper_clean_up_then_kills_it() {
    // One that exits when its stdin closes gets to (its cleanup runs).
    let tidy = Fake::new(
        "tidy",
        r#"
open(os.path.join(here, "pid"), "w").write(str(os.getpid()))
rec(b"REDY")
import threading
def frames():
    while True:
        frame(w, h, 1); time.sleep(1 / 30)
threading.Thread(target=frames, daemon=True).start()
sys.stdin.read()
time.sleep(0.3)
open(os.path.join(here, "clean"), "w").write("yes")
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(tidy.path(), &page(), video, None).unwrap();
    wait_for("pictures", 10, || seen.frames.load(Ordering::SeqCst) > 0);
    let pid = tidy.file("pid");
    drop(capture);
    assert_eq!(tidy.file("clean"), "yes", "it cleaned up before exiting");
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "helper {pid} is gone"
    );

    // One that ignores it is killed after the grace period.
    let stubborn = Fake::new(
        "stubborn",
        r#"
open(os.path.join(here, "pid"), "w").write(str(os.getpid()))
import signal
rec(b"REDY")
for n in range(100000):
    frame(w, h, 1); time.sleep(1 / 30)
"#,
    );
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(stubborn.path(), &page(), video, None).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let pid = stubborn.file("pid");
    let stopping = Instant::now();
    drop(capture);
    let took = stopping.elapsed();
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "helper {pid} is gone"
    );
    assert!(took >= EXIT_GRACE && took < EXIT_GRACE * 3, "{took:?}");
}

#[test]
fn doctor_explains_a_helper_without_its_sandbox_or_libraries() {
    use std::os::unix::process::ExitStatusExt;
    let output = |code: i32, stdout: &str, stderr: &str| {
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    };
    assert_eq!(sandbox_warning(output(0, "sandbox on\n", "")), None);
    let off = sandbox_warning(output(0, "sandbox off: namespaces are restricted\n", "")).unwrap();
    assert!(off.detail.contains("namespaces are restricted"), "{off:?}");
    assert!(off.fix.contains(".deb"), "{off:?}");
    let missing = sandbox_warning(output(
        127,
        "",
        "isocast-browser: error while loading shared libraries: libwebkitgtk-6.0.so.4: cannot open\n",
    ))
    .unwrap();
    assert!(
        missing.detail.contains("libwebkitgtk-6.0.so.4"),
        "{missing:?}"
    );
    assert!(missing.fix.contains("libwebkitgtk-6.0-4"), "{missing:?}");
    assert!(sandbox_warning(Err("it didn't answer within 5 s".into())).is_some());
}

#[test]
fn a_helper_over_the_memory_cap_is_restarted() {
    // 300 MB resident (written, so the pages are real) against a 128 MB
    // cap, still sending pictures: memory, not a hang, restarts it.
    let fake = Fake::new(
        "memory",
        r#"
rec(b"REDY")
hog = b"\x01" * (300 << 20) if starts == 0 else b""
for n in range(100000):
    frame(w, h, 1 if starts == 0 else 2); time.sleep(1 / 30)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start_capped(fake.path(), &page(), video, None, 128).unwrap();
    wait_for("pictures after the restart", 20, || {
        seen.first_byte.lock().unwrap().contains(&2)
    });
    let why = capture.shared.last_restart.lock().unwrap().clone();
    assert!(why.contains("over the 128 MB cap"), "{why}");
    assert_eq!(capture.shared.restarts.load(Ordering::SeqCst), 1);
}

#[test]
fn a_source_dropped_during_a_restart_stops_at_once() {
    // The first helper crashes; its replacement never says it is ready.
    let fake = Fake::new(
        "restart-stop",
        r#"
if starts == 0:
    rec(b"REDY")
    frame(w, h, 1)
    sys.exit(1)
time.sleep(3600)
"#,
    );
    let seen = Arc::new(Seen::default());
    let (video, _) = sinks(&seen);
    let (capture, _, _) = start(fake.path(), &page(), video, None).unwrap();
    wait_for("the restart", 10, || fake.starts() == 2);
    std::thread::sleep(Duration::from_millis(200)); // inside its wait for REDY
    let stopping = Instant::now();
    drop(capture);
    assert!(
        stopping.elapsed() < Duration::from_secs(2),
        "dropping waited {:?} (start timeout {:?})",
        stopping.elapsed(),
        START_TIMEOUT
    );
    assert!(
        seen.stopped.lock().unwrap().is_none(),
        "a drop isn't reported as a failure"
    );
}

#[test]
fn without_a_helper_browser_sources_say_they_need_gtk_4_10() {
    let warning = missing_helper(Path::new("/usr/bin/isocast-browser"));
    assert!(
        warning.detail.contains("/usr/bin/isocast-browser"),
        "{warning:?}"
    );
    assert!(warning.detail.contains("GTK 4.10"), "{warning:?}");
    assert!(warning.detail.contains("Debian 12"), "{warning:?}");
    assert!(warning.fix.contains("Ubuntu 24.04"), "{warning:?}");
}
