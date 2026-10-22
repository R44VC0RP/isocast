use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use mio::{Events, Interest, Poll, Token, Waker};

const CAPACITY: usize = 64 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const INPUT_STALL_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET: Token = Token(0);
const WAKE: Token = Token(1);

#[derive(Clone, Debug)]
pub struct Peer {
    pub client_id: String,
    pub name: String,
    pub addr: SocketAddr,
    pub spki: Vec<u8>,
}

/// Clones have independent read/write access, but share timeout and shutdown
/// state, just like the local stream. Plaintext and TLS buffers are bounded.
pub struct Duplex {
    shared: Arc<Shared>,
}

pub(crate) struct CloseHandle(Weak<Shared>);

impl CloseHandle {
    pub(crate) fn aborted(&self) -> bool {
        self.0
            .upgrade()
            .is_none_or(|shared| shared.state.lock().unwrap().aborted)
    }
    pub(crate) fn close(&self) {
        if let Some(shared) = self.0.upgrade() {
            let mut state = shared.state.lock().unwrap();
            state.stopped = true;
            state.aborted = true;
            state.read_closed = true;
            state.write_closed = true;
            state.incoming.clear();
            state.outgoing.clear();
            shared.changed.notify_all();
            let _ = shared.wake.wake();
        }
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    wake: Waker,
    handles: AtomicUsize,
    peer: Peer,
    exporter: [u8; 32],
}

#[derive(Default)]
struct State {
    incoming: VecDeque<u8>,
    read_progress: u64,
    outgoing: VecDeque<u8>,
    flushing: bool,
    read_closed: bool,
    peer_eof: bool,
    write_closed: bool,
    write_shutdown: Option<Instant>,
    stopped: bool,
    aborted: bool,
    closing: Option<Instant>,
    error: Option<(io::ErrorKind, String)>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    nonblocking: bool,
}

impl Duplex {
    pub(crate) fn start(
        socket: TcpStream,
        mut connection: rustls::Connection,
        peer: Peer,
    ) -> io::Result<Self> {
        let exporter = connection
            .export_keying_material([0u8; 32], b"EXPORTER-isocast-pair-v1", None)
            .map_err(io::Error::other)?;
        connection.set_buffer_limit(Some(CAPACITY));
        socket.set_nonblocking(true)?;
        let poll = Poll::new()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            wake: Waker::new(poll.registry(), WAKE)?,
            handles: AtomicUsize::new(1),
            peer,
            exporter,
        });
        let driver = shared.clone();
        std::thread::Builder::new()
            .name("remote TLS".into())
            .spawn(move || {
                let mut socket = mio::net::TcpStream::from_std(socket);
                let result = drive(&driver, &mut socket, connection, poll);
                let _ = socket.shutdown(Shutdown::Both);
                let mut state = driver.state.lock().unwrap();
                state.stopped = true;
                state.read_closed = true;
                state.write_closed = true;
                if let Err(error) = result {
                    state.error = Some((error.kind(), error.to_string()));
                }
                driver.changed.notify_all();
            })?;
        Ok(Self { shared })
    }

    pub fn peer(&self) -> &Peer {
        &self.shared.peer
    }
    pub(crate) fn close_handle(&self) -> CloseHandle {
        CloseHandle(Arc::downgrade(&self.shared))
    }
    pub fn exporter(&self) -> &[u8; 32] {
        &self.shared.exporter
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        self.shared.handles.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            shared: self.shared.clone(),
        })
    }
    pub fn hung_up(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.read_closed || state.peer_eof
    }
    /// Trust was revoked or the listener disabled, rather than a peer
    /// half-closing after sending a valid request. Buffered RPCs must not run.
    pub fn aborted(&self) -> bool {
        self.shared.state.lock().unwrap().aborted
    }
    pub fn readable(&self) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.read_closed || state.peer_eof || !state.incoming.is_empty()
    }
    /// Inspect the pairing discriminator without consuming ordinary RPC bytes.
    pub(crate) fn peek_byte(&self, timeout: Duration) -> io::Result<Option<u8>> {
        let mut state = self.shared.state.lock().unwrap();
        let deadline = Some(Instant::now() + timeout);
        loop {
            if let Some(byte) = state.incoming.front() {
                return Ok(Some(*byte));
            }
            if let Some((kind, message)) = &state.error {
                return Err(io::Error::new(*kind, message.clone()));
            }
            if state.read_closed || state.peer_eof {
                return Ok(None);
            }
            state = self.wait(state, deadline)?;
        }
    }
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        self.shared.state.lock().unwrap().read_timeout = timeout;
        Ok(())
    }
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        validate_timeout(timeout)?;
        self.shared.state.lock().unwrap().write_timeout = timeout;
        Ok(())
    }
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.shared.state.lock().unwrap().nonblocking = nonblocking;
        Ok(())
    }
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if matches!(how, Shutdown::Read | Shutdown::Both) {
            state.read_closed = true;
            state.incoming.clear();
        }
        if matches!(how, Shutdown::Write | Shutdown::Both) {
            state.write_closed = true;
            state
                .write_shutdown
                .get_or_insert_with(|| Instant::now() + SHUTDOWN_TIMEOUT);
        }
        if how == Shutdown::Both {
            // Normal stream shutdown preserves bytes write() already accepted.
            // Only CloseHandle (revocation/disable) aborts and discards them.
            state
                .closing
                .get_or_insert_with(|| Instant::now() + SHUTDOWN_TIMEOUT);
        }
        self.shared.changed.notify_all();
        self.shared.wake.wake()
    }
    fn wait<'a>(
        &self,
        state: MutexGuard<'a, State>,
        deadline: Option<Instant>,
    ) -> io::Result<MutexGuard<'a, State>> {
        if state.nonblocking {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        match deadline {
            Some(deadline) => {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or(io::ErrorKind::TimedOut)?;
                let (state, result) = self.shared.changed.wait_timeout(state, remaining).unwrap();
                if result.timed_out() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Ok(state)
            }
            None => Ok(self.shared.changed.wait(state).unwrap()),
        }
    }
}

impl Drop for Duplex {
    fn drop(&mut self) {
        if self.shared.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut state = self.shared.state.lock().unwrap();
            state.read_closed = true;
            state.write_closed = true;
            state
                .closing
                .get_or_insert_with(|| Instant::now() + SHUTDOWN_TIMEOUT);
            let _ = self.shared.wake.wake();
        }
    }
}

impl Read for Duplex {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.state.lock().unwrap();
        let deadline = state.read_timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if !state.incoming.is_empty() {
                let count = bytes.len().min(state.incoming.len());
                for byte in &mut bytes[..count] {
                    *byte = state.incoming.pop_front().unwrap();
                }
                state.read_progress = state.read_progress.wrapping_add(count as u64);
                drop(state);
                self.shared.wake.wake()?;
                return Ok(count);
            }
            if let Some((kind, message)) = &state.error {
                return Err(io::Error::new(*kind, message.clone()));
            }
            if state.read_closed || state.peer_eof {
                return Ok(0);
            }
            state = self.wait(state, deadline)?;
        }
    }
}

impl Write for Duplex {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut state = self.shared.state.lock().unwrap();
        let deadline = state.write_timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if state.write_closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let count = bytes.len().min(CAPACITY - state.outgoing.len());
            if count != 0 {
                state.outgoing.extend(&bytes[..count]);
                drop(state);
                self.shared.wake.wake()?;
                return Ok(count);
            }
            state = self.wait(state, deadline)?;
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        let deadline = state.write_timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if state.stopped {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if state.outgoing.is_empty() && !state.flushing {
                return Ok(());
            }
            state = self.wait(state, deadline)?;
        }
    }
}

fn validate_timeout(timeout: Option<Duration>) -> io::Result<()> {
    if timeout == Some(Duration::ZERO) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "timeout must be positive",
        ));
    }
    Ok(())
}

fn drive(
    shared: &Shared,
    socket: &mut mio::net::TcpStream,
    mut tls: rustls::Connection,
    mut poll: Poll,
) -> io::Result<()> {
    let mut events = Events::with_capacity(8);
    let mut registered = false;
    let mut close_sent = false;
    let mut eof = false;
    let mut stalled = None;
    let mut read_progress = 0;
    loop {
        let mut state = shared.state.lock().unwrap();
        if state.stopped {
            return Ok(());
        }
        if [state.closing, state.write_shutdown]
            .into_iter()
            .flatten()
            .min()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS shutdown could not drain accepted output before its deadline",
            ));
        }
        // Feed at most one bounded pipe into rustls; writer() observes its
        // buffer limit even if the socket is not accepting encrypted bytes.
        while !state.outgoing.is_empty() {
            let (first, _) = state.outgoing.as_slices();
            match tls.writer().write(first) {
                Ok(0) => break,
                Ok(count) => {
                    state.outgoing.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        if state.write_closed && state.outgoing.is_empty() && !close_sent {
            tls.send_close_notify();
            close_sent = true;
        }
        state.flushing = tls.wants_write();
        drop(state);

        while tls.wants_write() {
            match tls.write_tls(socket) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        let mut state = shared.state.lock().unwrap();
        state.flushing = tls.wants_write();
        if close_sent && !state.flushing {
            state.write_shutdown = None;
        }
        if state.closing.is_some() && close_sent && !state.flushing && state.outgoing.is_empty() {
            return Ok(());
        }
        let mut bytes = [0u8; 16384];
        while !eof {
            let room = if state.read_closed {
                bytes.len()
            } else {
                bytes.len().min(CAPACITY - state.incoming.len())
            };
            if room == 0 {
                break;
            }
            match tls.reader().read(&mut bytes[..room]) {
                Ok(0) => {
                    eof = true;
                    state.peer_eof = true;
                    break;
                }
                Ok(count) => {
                    if !state.read_closed {
                        state.incoming.extend(&bytes[..count]);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        shared.changed.notify_all();
        if eof && close_sent && !state.flushing && state.outgoing.is_empty() {
            return Ok(());
        }
        // A full incoming pipe must not park the driver forever with neither
        // socket interest enabled: even a TCP failure cannot wake that poll.
        // Give the application five seconds to consume some buffered input.
        if state.read_progress != read_progress {
            read_progress = state.read_progress;
            stalled = None;
        }
        if !state.read_closed && state.incoming.len() == CAPACITY {
            let deadline = *stalled.get_or_insert_with(|| Instant::now() + INPUT_STALL_TIMEOUT);
            if deadline <= Instant::now() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote plaintext reader stalled",
                ));
            }
        } else {
            stalled = None;
        }
        let read =
            !eof && tls.wants_read() && (state.read_closed || state.incoming.len() < CAPACITY);
        let write = tls.wants_write();
        // Plaintext may have been left behind by the previous TLS buffer cap.
        let pending = !write && !state.outgoing.is_empty();
        let deadline = [state.closing, state.write_shutdown, stalled]
            .into_iter()
            .flatten()
            .min();
        let wait = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        drop(state);
        if pending {
            continue;
        }
        let interest = match (read, write) {
            (true, true) => Some(Interest::READABLE | Interest::WRITABLE),
            (true, false) => Some(Interest::READABLE),
            (false, true) => Some(Interest::WRITABLE),
            (false, false) => None,
        };
        match (registered, interest) {
            (true, Some(interest)) => poll.registry().reregister(socket, SOCKET, interest)?,
            (false, Some(interest)) => {
                poll.registry().register(socket, SOCKET, interest)?;
                registered = true;
            }
            (true, None) => {
                poll.registry().deregister(socket)?;
                registered = false;
            }
            (false, None) => {}
        }
        match poll.poll(&mut events, wait) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        for event in &events {
            if event.token() != SOCKET || !read || !(event.is_readable() || event.is_read_closed())
            {
                continue;
            }
            // Drain readiness until rustls' bounded plaintext buffer applies
            // backpressure. A subsequent reregister re-arms edge readiness.
            while tls.wants_read() {
                match tls.read_tls(socket) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "TLS peer disconnected without close_notify",
                        ));
                    }
                    Ok(_) => {
                        let status = tls.process_new_packets().map_err(io::Error::other)?;
                        if status.peer_has_closed() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }
}
