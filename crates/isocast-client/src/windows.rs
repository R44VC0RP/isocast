//! Detached daemon launch with an explicit handle allow-list. std::Command on
//! stable Rust inherits every inheritable handle: redirecting stdio alone still
//! leaked the caller's captured pipes into the daemon and held CLI exits open.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::ExitStatus;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Globalization::{
    CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::isolated::SpawnOptions;

struct Attributes<'a> {
    storage: Vec<u128>,
    _handles: &'a [HANDLE],
}

impl<'a> Attributes<'a> {
    fn new(handles: &'a [HANDLE]) -> io::Result<Self> {
        let mut bytes = 0;
        // The first call only asks for storage size; failure with a required
        // size is the documented query form.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0u128; bytes.div_ceil(size_of::<u128>())];
        let list = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut result = Self {
            storage,
            _handles: handles,
        };
        // SAFETY: aligned initialized list and a live handle array. Both stay
        // alive through CreateProcessW; only these handles may cross the boundary.
        if unsafe {
            UpdateProcThreadAttribute(
                result.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(handles),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(result)
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

fn inheritable(handle: HANDLE) -> io::Result<OwnedHandle> {
    let mut copy = null_mut();
    // SAFETY: borrowed current-process handles; DuplicateHandle returns a new
    // owned handle. The original file handle's inheritance flag never changes.
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut copy,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(copy) })
}

pub(super) fn spawn_daemon(daemon: &Path, log: &Path) -> io::Result<()> {
    // Dropping the process handle does not stop the background daemon.
    spawn_daemon_with(daemon, log, None).map(|_| ())
}

pub(super) fn spawn_daemon_with(
    daemon: &Path,
    log: &Path,
    isolated: Option<&SpawnOptions>,
) -> io::Result<Child> {
    let mut program: Vec<u16> = daemon.as_os_str().encode_wide().collect();
    if program.is_empty() || program.iter().any(|&c| c == 0 || c == u16::from(b'"')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid daemon path",
        ));
    }
    let mut command = vec![u16::from(b'"')];
    command.extend_from_slice(&program);
    command.push(u16::from(b'"'));
    if isolated.is_some() {
        // A fixed flag, with no backslashes or embedded quotes to escape.
        command.extend(" \"--exit-on-stdin-close\"".encode_utf16());
    }
    command.push(0);
    program.push(0);
    // Keep the normal branch's inherited environment (null pointer) unchanged.
    let environment = isolated.map(environment).transpose()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let output = inheritable(log.as_raw_handle())?;
    let input = match isolated {
        Some(options) => inheritable(options.input.as_raw_handle())?,
        None => inheritable(std::fs::File::open("NUL")?.as_raw_handle())?,
    };
    let handles = [input.as_raw_handle(), output.as_raw_handle()];
    let mut attributes = Attributes::new(&handles)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[1];
    startup.lpAttributeList = attributes.as_mut_ptr();
    let mut process = PROCESS_INFORMATION::default();
    // Break away from the CLI's console AND job (including SSH). Never silently
    // retry attached to a job which could kill a recording when its caller exits.
    // SAFETY: all strings, stdio handles, attribute storage and outputs are live
    // for the synchronous call. Only the reader and log are inheritable; the
    // ownership pipe's writer remains private and is never in HANDLE_LIST.
    if unsafe {
        CreateProcessW(
            program.as_ptr(),
            command.as_mut_ptr(),
            null(),
            null(),
            1,
            DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_BREAKAWAY_FROM_JOB
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            environment
                .as_ref()
                .map_or(null(), |block| block.as_ptr().cast()),
            null(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // Closing these handles does not stop the child; Windows reaps it at exit.
    let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    Ok(Child(unsafe {
        OwnedHandle::from_raw_handle(process.hProcess)
    }))
}

pub(super) struct Child(OwnedHandle);

impl Child {
    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        use std::os::windows::process::ExitStatusExt;
        // The process handle identifies the actual child, even after its PID
        // has been reused. Query its exit code only AFTER it becomes signaled.
        if unsafe { WaitForSingleObject(self.0.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0;
        if unsafe { GetExitCodeProcess(self.0.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ExitStatus::from_raw(code))
    }
}

// Windows environment keys use the OS's case folding, not Rust's Unicode
// uppercasing or locale-sensitive ordering (the standard library does likewise).
#[derive(Eq)]
struct EnvironmentKey(Vec<u16>);

impl Ord for EnvironmentKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // Both strings have validated i32 lengths. With valid arguments this
        // comparison cannot fail, and it preserves unpaired UTF-16 surrogates.
        match unsafe {
            CompareStringOrdinal(
                self.0.as_ptr(),
                self.0.len() as i32,
                other.0.as_ptr(),
                other.0.len() as i32,
                1,
            )
        } {
            CSTR_LESS_THAN => Ordering::Less,
            CSTR_EQUAL => Ordering::Equal,
            CSTR_GREATER_THAN => Ordering::Greater,
            _ => panic!(
                "could not compare environment keys: {}",
                io::Error::last_os_error()
            ),
        }
    }
}

impl PartialOrd for EnvironmentKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EnvironmentKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

fn environment(options: &SpawnOptions) -> io::Result<Vec<u16>> {
    fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
        let value: Vec<_> = value.encode_wide().collect();
        if value.contains(&0) || value.len() > i32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid isolated daemon environment",
            ));
        }
        Ok(value)
    }

    let mut entries = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        entries.insert(EnvironmentKey(wide(&key)?), wide(&value)?);
    }
    for (key, value) in &options.env {
        entries.insert(
            EnvironmentKey(wide(OsStr::new(key))?),
            wide(value.as_os_str())?,
        );
    }
    let mut block = Vec::new();
    for (key, value) in entries {
        block.extend(key.0);
        block.push(u16::from(b'='));
        block.extend(value);
        block.push(0);
    }
    block.push(0);
    Ok(block)
}
