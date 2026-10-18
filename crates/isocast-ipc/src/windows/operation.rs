use std::cell::UnsafeCell;
use std::io;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, GetLastError, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE};

use super::{Pipe, owned, win_error};

/// An operation cannot outlive its pipe. The boxed cell stays at a stable
/// address and permits the kernel to update it while Rust waits. Drop always
/// drains cancellation before freeing either the OVERLAPPED or its event.
pub(super) struct Operation<'a> {
    pipe: &'a Pipe,
    overlapped: Box<UnsafeCell<OVERLAPPED>>,
    _event: OwnedHandle,
    pending: bool,
}

impl<'a> Operation<'a> {
    pub(super) fn new(pipe: &'a Pipe) -> io::Result<Self> {
        // SAFETY: an unnamed, non-inheritable, manual-reset event.
        let event = unsafe { owned(CreateEventW(null(), 1, 0, null()))? };
        let overlapped = Box::new(UnsafeCell::new(OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..OVERLAPPED::default()
        }));
        Ok(Self {
            pipe,
            overlapped,
            _event: event,
            pending: false,
        })
    }

    /// SAFETY: submit exactly one overlapped operation on self.pipe. Any buffers
    /// supplied to the OS must remain borrowed and valid until wait or drop has
    /// completed. The closure must only submit the call, without panicking or
    /// calling another Win32 API before returning its BOOL result.
    pub(super) unsafe fn submit(
        &mut self,
        start: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> Result<(), u32> {
        let success = start(self.overlapped.get());
        if success == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                // Submission errors need call-site context: ERROR_NO_DATA is an
                // empty NOWAIT read, but a closed pipe for other operations.
                return Err(code);
            }
        }
        self.pending = true;
        Ok(())
    }

    pub(super) fn wait(&mut self, timeout: Option<Duration>) -> io::Result<u32> {
        let started = Instant::now();
        loop {
            let milliseconds = timeout.map_or(INFINITE, |timeout| {
                timeout
                    .saturating_sub(started.elapsed())
                    .as_nanos()
                    .div_ceil(1_000_000)
                    .min((INFINITE - 1) as u128) as u32
            });
            let mut bytes = 0;
            // SAFETY: private manual-reset event, live pipe/OVERLAPPED/buffer.
            let success = unsafe {
                GetOverlappedResultEx(
                    self.pipe.raw(),
                    self.overlapped.get(),
                    &mut bytes,
                    milliseconds,
                    0,
                )
            };
            if success != 0 {
                self.pending = false;
                return Ok(bytes);
            }
            let code = unsafe { GetLastError() };
            if matches!(code, WAIT_TIMEOUT | ERROR_IO_INCOMPLETE) {
                if timeout.is_some_and(|timeout| started.elapsed() < timeout) {
                    continue;
                }
                // A successful completion can race the timeout. Preserve its
                // byte count instead of losing data or reporting a false retry.
                return match self.cancel_and_wait() {
                    Ok(bytes) => Ok(bytes),
                    Err(ERROR_OPERATION_ABORTED) => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "local pipe I/O timed out",
                    )),
                    Err(code) => Err(win_error(code)),
                };
            }
            // Even unexpected wait failures must not free a pending pointer.
            let _ = self.cancel_and_wait();
            return Err(win_error(code));
        }
    }

    fn cancel_and_wait(&mut self) -> Result<u32, u32> {
        // CancelIoEx only REQUESTS cancellation; even ERROR_NOT_FOUND can mean
        // completion raced us. Always wait for the final result before freeing.
        unsafe { CancelIoEx(self.pipe.raw(), self.overlapped.get()) };
        loop {
            let mut bytes = 0;
            let success = unsafe {
                GetOverlappedResult(self.pipe.raw(), self.overlapped.get(), &mut bytes, 1)
            };
            if success != 0 {
                self.pending = false;
                return Ok(bytes);
            }
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_INCOMPLETE {
                self.pending = false;
                return Err(code);
            }
        }
    }
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if self.pending {
            let _ = self.cancel_and_wait();
        }
    }
}
