//! The Windows part of browser sources: the kill-on-close job and the
//! advice. The host's process handling (crash, hang, restart, sizes, pools)
//! is tested in crates/isocast-browser-host; the real helper runs in the
//! e2e scenario `win_browser`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use isocast_engine::PageLocation;
use windows::Win32::Foundation::STILL_ACTIVE;
use windows::Win32::System::Threading::GetExitCodeProcess;

use super::*;

fn python() -> PathBuf {
    std::env::var_os("PYTHON").map_or_else(|| "python".into(), PathBuf::from)
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

fn wait_for(what: &str, seconds: u64, ok: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
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
    let text = explain_start_failure(Some(7), None, "something went wrong\n");
    assert!(
        text.contains("status 0x7") && text.contains("something went wrong"),
        "{text}"
    );
}

#[test]
fn memory_of_a_job_counts_its_processes() {
    let job = Job::new().unwrap();
    let mut child = Command::new(python())
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

/// CPU frames, for a source without a GPU.
struct Pool;

impl FramePool for Pool {
    type Frame = Vec<u8>;
    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Vec<u8>>> {
        let mut pixels = vec![0u8; 8 * 4 * 4];
        draw(&mut pixels, 8 * 4);
        Ok(Some(pixels))
    }
    fn take(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[test]
fn the_helpers_own_processes_end_with_the_source() {
    // WebView2's browser processes are the helper's children: they share
    // its job and end with it, even if the helper never cleans up.
    let dir = std::env::temp_dir().join(format!("isocast-browser-job-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("helper.py");
    std::fs::write(
        &script,
        r#"
import os, struct, subprocess, sys, time
out = sys.stdout.buffer
def rec(tag, payload=b""):
    out.write(tag + struct.pack("<I", len(payload)) + payload); out.flush()
sys.stdin.readline()
here = os.path.dirname(os.path.abspath(__file__))
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(3600)"])
open(os.path.join(here, "child"), "w").write(str(child.pid))
rec(b"REDY")
while True:
    rec(b"FRAM", struct.pack("<II", 8, 4) + bytes(8 * 4 * 4)); time.sleep(1 / 30)
"#,
    )
    .unwrap();
    let helper = Helper {
        program: python(),
        prefix: vec!["-u".into(), script.to_string_lossy().into_owned()],
    };
    let frames = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(Mutex::new(None::<String>));
    let (f, s) = (frames.clone(), stopped.clone());
    let sink = VideoSink {
        frame: Box::new(move |_: Vec<u8>, _| {
            f.fetch_add(1, Ordering::SeqCst);
        }),
        stopped: Box::new(move |why| *s.lock().unwrap() = Some(why)),
    };
    let page = BrowserPage {
        location: PageLocation::Url("http://example.invalid/"),
        width: 8,
        height: 4,
        fps: 30,
        css: "",
    };
    let (capture, _, _) = start(helper, &page, || Ok(Pool), sink, None).unwrap();
    let child = dir.join("child");
    wait_for("the child", 10, || {
        child.exists() && frames.load(Ordering::SeqCst) > 0
    });
    let pid: u32 = std::fs::read_to_string(&child)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid), "the helper's child runs");
    drop(capture);
    wait_for("the child to end", 10, || !alive(pid));
    assert!(stopped.lock().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
