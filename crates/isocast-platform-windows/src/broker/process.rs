use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use windows::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS,
    SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, GetCurrentProcess, TerminateProcess, WaitForSingleObject,
};
use windows::core::Owned;

use crate::native::SharedHandle;

struct Job(Owned<HANDLE>);

// SAFETY: an owned, unnamed kernel job, never inherited or exposed to a child.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

/// Owns the real Child handle throughout assignment, startup, streaming and
/// teardown. No process lookup by PID, no OpenProcess and no public endpoint.
pub(super) struct Process {
    child: Mutex<Child>,
    job: Job,
    // Borrowed from child; child is never taken out of its mutex. Stored as an
    // integer only for Send/Sync; every use holds a reference to this owner.
    handle: usize,
    shut_down: AtomicBool,
}

impl Process {
    pub(super) fn spawn(path: &Path) -> Result<(Self, ChildStdin, ChildStdout)> {
        if !path.is_absolute() || !path.is_file() {
            bail!(
                "capture helper missing: install isocast-capture.exe beside the executable (explicit helper paths must be absolute)"
            );
        }
        let job = Job(unsafe { Owned::new(CreateJobObjectW(None, None)?) });
        unsafe { SetHandleInformation(*job.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
            .context("could not make the capture helper job non-inheritable")?;
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
        .context("could not configure the capture helper job")?;
        // Command manages the configured pipe ends. The job and our resource
        // handles are explicitly non-inheritable; do not rely on Command to
        // filter unrelated inheritable handles created elsewhere in the host.
        let child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .context("could not start isocast-capture.exe")?;
        let handle = child.as_raw_handle() as usize;
        let process = Self {
            child: Mutex::new(child),
            job,
            handle,
            shut_down: AtomicBool::new(false),
        };
        // The child cannot create a GPU or call WGC until Init, which is sent
        // only after this succeeds. Its independent no-Init/EOF guard covers a
        // parent crash in the small spawn-to-assignment window.
        unsafe { AssignProcessToJobObject(*process.job.0, process.handle()) }
            .context("could not assign capture helper to its kill-on-close job; nested jobs may be unavailable")?;
        let input = process
            .child
            .lock()
            .stdin
            .take()
            .context("capture helper stdin missing")?;
        let output = process
            .child
            .lock()
            .stdout
            .take()
            .context("capture helper stdout missing")?;
        Ok((process, input, output))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.handle as *mut _)
    }

    pub(super) fn duplicate(&self, borrowed: u64) -> Result<SharedHandle> {
        let mut owned = HANDLE::default();
        // Spawn supplies the PROCESS_DUP_HANDLE right. Borrowed handles are
        // never closed here; only successfully duplicated parent handles are.
        unsafe {
            DuplicateHandle(
                self.handle(),
                HANDLE(borrowed as usize as *mut _),
                GetCurrentProcess(),
                &mut owned,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .context("capture helper exited or its shared resource was unavailable")?;
        unsafe { SharedHandle::new(owned) }
    }

    pub(super) fn kill(&self) {
        let _ = unsafe { TerminateJobObject(*self.job.0, 1) };
        // Also covers assignment failure: this is ONLY our owned Child handle.
        let _ = unsafe { TerminateProcess(self.handle(), 1) };
    }

    pub(super) fn shutdown(&self, graceful: bool) {
        if self.shut_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let ended = unsafe { WaitForSingleObject(self.handle(), if graceful { 250 } else { 0 }) };
        if ended != WAIT_OBJECT_0 {
            self.kill();
            // Never wait on WGC/COM teardown. Windows termination is bounded
            // here too; the kill-on-close job remains owned on timeout.
            let _ = unsafe { WaitForSingleObject(self.handle(), 2000) };
        }
        // Reap without taking/dropping Child. Receiver's Arc<Control> keeps
        // this exact process handle pinned across concurrent DuplicateHandle,
        // including after termination; its numeric value cannot be recycled.
        let _ = self.child.lock().try_wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.shutdown(false);
    }
}
