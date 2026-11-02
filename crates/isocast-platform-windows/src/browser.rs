//! Browser sources: each page renders in its own `isocast-browser.exe`
//! helper process (a WebView2 composition controller whose visual is
//! captured offscreen; see crates/isocast-browser). crates/isocast-browser-host
//! starts and watches it; this module is the Windows part: the helper's
//! command, and a kill-on-close job holding the helper and every process it
//! starts (WebView2's browser, renderers, GPU process), so they all end with
//! the source or with isocastd and their memory is counted together.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use isocast_browser_host::{BrowserCapture, Confinement, HelperOs, Timing};
use isocast_engine::{
    AudioSink, BrowserControl, BrowserPage, CaptureFormat, FramePool, Time, VideoSink,
};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
};
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

impl HelperOs for Helper {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.prefix)
            .creation_flags(CREATE_NO_WINDOW.0);
        command
    }

    fn adopt(&self, child: &Child) -> Result<Box<dyn Confinement>> {
        let job = Job::new()?;
        // Before the page line is written: the helper starts WebView2 only
        // after reading it, so its children land in the job too.
        unsafe { AssignProcessToJobObject(*job.0, HANDLE(child.as_raw_handle())) }
            .map_err(|err| anyhow!(err))
            .context(
                "could not put the browser helper in its kill-on-close job; nested jobs may be unavailable",
            )?;
        Ok(Box::new(job))
    }

    fn now(&self) -> Time {
        crate::now()
    }

    fn explain_start_failure(
        &self,
        status: Option<i32>,
        reason: Option<&str>,
        stderr: &str,
    ) -> String {
        explain_start_failure(status, reason, stderr)
    }
}

/// Start rendering `page` in `helper`, delivering pictures drawn into pools
/// from `make_pool`. Fails, with the fix, if the helper can't load the page
/// at all (no WebView2 Runtime, no desktop to capture on).
pub(crate) fn start<P, F>(
    helper: Helper,
    page: &BrowserPage<'_>,
    make_pool: F,
    sink: VideoSink<P::Frame>,
    audio: Option<AudioSink>,
) -> Result<(BrowserCapture, CaptureFormat, Arc<dyn BrowserControl>)>
where
    P: FramePool + 'static,
    P::Frame: Send + 'static,
    F: FnMut() -> Result<P> + Send + 'static,
{
    isocast_browser_host::start(
        Arc::new(helper),
        Timing::default(),
        page,
        make_pool,
        sink,
        audio,
    )
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

impl Confinement for Job {
    fn kill(&self) {
        Job::kill(self);
    }

    fn memory_mb(&self) -> u64 {
        Job::memory_mb(self)
    }
}

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

#[cfg(test)]
mod tests;
