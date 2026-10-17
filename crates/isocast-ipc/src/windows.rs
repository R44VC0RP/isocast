use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_NO_DATA, ERROR_NOT_FOUND,
    ERROR_OPERATION_ABORTED, ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_LISTENING, ERROR_PIPE_NOT_CONNECTED, ERROR_SEM_TIMEOUT, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW,
};

mod name;
mod operation;
mod security;

use operation::Operation;
use security::{Peer, User};

/// A local, owner-only listener. The logical path's parent must exist.
pub struct Listener {
    name: Vec<u16>,
    user: User,
    // Established with FIRST_PIPE_INSTANCE at bind. Always retain an instance:
    // create its successor before handing it off, and keep it on any failure.
    // The kernel namespace reservation thus never lapses between accepts.
    waiting: Mutex<Pipe>,
}

impl Listener {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let name = name::pipe_name(path.as_ref())?;
        let user = User::current()?;
        let waiting = user.create_pipe(&name, true)?;
        Ok(Self {
            name,
            user,
            waiting: Mutex::new(waiting),
        })
    }

    pub fn incoming(&self) -> Incoming<'_> {
        Incoming(self)
    }

    fn accept(&self) -> io::Result<Stream> {
        let mut waiting = lock(&self.waiting);
        let result = (|| {
            waiting.accept()?;
            let peer = self.user.verify_peer(&waiting)?;
            let next = self.user.create_pipe(&self.name, false)?;
            let pipe = std::mem::replace(&mut *waiting, next);
            Ok(Stream::new(pipe, peer))
        })();
        if result.is_err() {
            // No payload was exchanged. Eject even a connected client on any
            // failure, but retain the server handle and its name reservation.
            // Connect's OVERLAPPED has already been drained before we get here.
            unsafe { DisconnectNamedPipe(waiting.raw()) };
        }
        result
    }
}

pub struct Incoming<'a>(&'a Listener);

impl Iterator for Incoming<'_> {
    type Item = io::Result<Stream>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.accept())
    }
}

/// Clones share the connection, shutdown state and timeout settings. One read
/// and one write may run concurrently; operations in each direction serialize.
pub struct Stream(Arc<State>);

struct State {
    // This lock covers submission, not waiting. Taking the pipe in shutdown
    // prevents any submission from slipping past CancelIoEx.
    pipe: Mutex<Option<Arc<Pipe>>>,
    read: Mutex<()>,
    write: Mutex<()>,
    timeouts: Mutex<Timeouts>,
    _peer: Peer,
}

#[derive(Default)]
struct Timeouts {
    read: Option<Duration>,
    write: Option<Duration>,
}

impl Stream {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let name = name::pipe_name(path.as_ref())?;
        let user = User::current()?;
        let pipe = Pipe::connect(&name)?;
        let peer = user.verify_peer(&pipe)?;
        Ok(Self::new(pipe, peer))
    }

    fn new(pipe: Pipe, peer: Peer) -> Self {
        Self(Arc::new(State {
            pipe: Mutex::new(Some(Arc::new(pipe))),
            read: Mutex::new(()),
            write: Mutex::new(()),
            timeouts: Mutex::new(Timeouts::default()),
            _peer: peer,
        }))
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(Arc::clone(&self.0)))
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        lock(&self.0.timeouts).read = timeout;
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        lock(&self.0.timeouts).write = timeout;
        Ok(())
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if how != Shutdown::Both {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named pipes support only Shutdown::Both",
            ));
        }
        let mut state = lock(&self.0.pipe);
        let Some(pipe) = state.take() else {
            return Ok(());
        };
        // SAFETY: the owned pipe stays alive through cancellation and both
        // direction locks below. No new I/O can start after state.take().
        let cancelled = unsafe { CancelIoEx(pipe.raw(), null()) };
        if cancelled == 0 {
            // ERROR_NOT_FOUND means all submitted operations already finished.
            let code = unsafe { GetLastError() };
            if code != ERROR_NOT_FOUND {
                // Cancellation failed: keep the stream usable and let the
                // outstanding operations retain/drain their own pipe references.
                // Waiting on their direction locks here could block indefinitely.
                *state = Some(pipe);
                return Err(win_error(code));
            }
        }
        drop(state);
        // Each operation owns a pipe reference and drains its OVERLAPPED before
        // releasing its direction lock. Do not close a handle still used by I/O.
        let _read = lock(&self.0.read);
        let _write = lock(&self.0.write);
        drop(pipe);
        Ok(())
    }

    fn read_inner(&self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let _read = lock(&self.0.read);
        let timeout = lock(&self.0.timeouts).read;
        let state = lock(&self.0.pipe);
        let Some(pipe) = state.as_ref().cloned() else {
            return Ok(0);
        };
        let mut operation = Operation::new(&pipe)?;
        // SAFETY: buffer stays exclusively borrowed until operation completes
        // or its drop cancels AND drains it, on every exit (including unwinding).
        let submitted = unsafe {
            operation.submit(|overlapped| {
                ReadFile(
                    pipe.raw(),
                    buffer.as_mut_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    overlapped,
                )
            })
        };
        drop(state);
        match submitted.and_then(|()| operation.wait(timeout)) {
            Ok(bytes) => Ok(bytes as usize),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn write_inner(&self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let _write = lock(&self.0.write);
        let timeout = lock(&self.0.timeouts).write;
        let state = lock(&self.0.pipe);
        let pipe = state.as_ref().cloned().ok_or_else(closed)?;
        let mut operation = Operation::new(&pipe)?;
        // SAFETY: buffer remains borrowed and unchanged until completion or the
        // operation's cancel-and-drain drop. The state lock serializes shutdown.
        let submitted = unsafe {
            operation.submit(|overlapped| {
                WriteFile(
                    pipe.raw(),
                    buffer.as_ptr(),
                    buffer.len().min(u32::MAX as usize) as u32,
                    null_mut(),
                    overlapped,
                )
            })
        };
        drop(state);
        submitted?;
        operation.wait(timeout).map(|bytes| bytes as usize)
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_inner(buffer)
    }
}

impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_inner(buffer)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_inner(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        // No userspace buffering. FlushFileBuffers would instead wait for the
        // peer to consume everything, with no overlapped timeout support.
        Ok(())
    }
}

impl Write for &Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_inner(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Query the connected peer, not a PID supplied in a protocol message.
pub fn peer_pid(stream: &Stream) -> io::Result<u32> {
    let state = lock(&stream.0.pipe);
    state.as_ref().ok_or_else(closed)?.peer_pid()
}

/// Check an overlapped pipe without reading or waiting for a message.
pub fn hung_up(stream: &Stream) -> bool {
    let state = lock(&stream.0.pipe);
    let Some(pipe) = state.as_ref() else {
        return true;
    };
    // SAFETY: no output buffer; the handle is live and opened overlapped, so
    // PeekNamedPipe does not serialize behind a blocking synchronous ReadFile.
    unsafe {
        PeekNamedPipe(
            pipe.raw(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
        ) == 0
    }
}

struct Pipe {
    handle: OwnedHandle,
    server: bool,
}

impl Pipe {
    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    fn connect(name: &[u16]) -> io::Result<Self> {
        // SAFETY: NUL-terminated name. No inheritable handle, and identification
        // SQOS prevents even a malicious server from impersonating this client.
        let deadline = Instant::now() + Duration::from_secs(1);
        let handle = loop {
            let result = unsafe {
                owned(CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    null_mut(),
                ))
            };
            match result {
                Ok(handle) => break handle,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    // Availability is only a hint: another client can win the
                    // instance before CreateFileW, so retry within one deadline.
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .clamp(1, 50) as u32;
                    if unsafe { WaitNamedPipeW(name.as_ptr(), remaining) } == 0 {
                        let code = unsafe { GetLastError() };
                        if code != ERROR_SEM_TIMEOUT {
                            return Err(win_error(code));
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        };
        Ok(Self {
            handle,
            server: false,
        })
    }

    fn accept(&self) -> io::Result<()> {
        let mut operation = Operation::new(self)?;
        // SAFETY: this instance is exclusively owned by the acceptor. No payload
        // is read or written until the peer's kernel identity has been checked.
        match unsafe { operation.submit(|overlapped| ConnectNamedPipe(self.raw(), overlapped)) } {
            Ok(()) => operation.wait(None).map(|_| ()),
            // A client can connect between CreateNamedPipe and ConnectNamedPipe.
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn peer_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        // SAFETY: valid pipe and output pointer. The API depends on our end of
        // the connection, not on any claimed identity from the peer.
        let success = unsafe {
            if self.server {
                GetNamedPipeClientProcessId(self.raw(), &mut pid)
            } else {
                GetNamedPipeServerProcessId(self.raw(), &mut pid)
            }
        };
        if success == 0 {
            return Err(last_error());
        }
        Ok(pid)
    }
}

// Do not DisconnectNamedPipe on normal stream drop: it discards unread bytes.
// OwnedHandle closes our end instead, without an unbounded peer-draining flush.

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn validate_timeout(timeout: Option<Duration>) -> io::Result<()> {
    if timeout.is_some_and(|timeout| timeout.is_zero()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a pipe timeout must be nonzero",
        ));
    }
    Ok(())
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "local pipe is closed")
}

// SAFETY: callers pass only fresh handles returned by owning Win32 APIs,
// never borrowed/pseudo handles. Both documented failure sentinels are rejected.
unsafe fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn last_error() -> io::Error {
    win_error(unsafe { GetLastError() })
}

fn win_error(code: u32) -> io::Error {
    let error = io::Error::from_raw_os_error(code as i32);
    let kind = match code {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => io::ErrorKind::NotFound,
        ERROR_PIPE_BUSY | ERROR_PIPE_LISTENING => io::ErrorKind::WouldBlock,
        WAIT_TIMEOUT => io::ErrorKind::TimedOut,
        ERROR_BROKEN_PIPE
        | ERROR_PIPE_NOT_CONNECTED
        | ERROR_NO_DATA
        | ERROR_HANDLE_EOF
        | ERROR_OPERATION_ABORTED => io::ErrorKind::BrokenPipe,
        _ => return error,
    };
    io::Error::new(kind, error)
}
