//! The daemon's byte-stream transport. Local operations retain the standard
//! Unix socket / authenticated Windows named-pipe behavior without buffering.
//!
//! Windows pipe names include the process user's binary SID and the normalized
//! logical path. This namespace replaced path-only names: restart existing
//! Windows development daemons when upgrading; there is no old-name fallback.
//! A SID is not secret. Namespace isolation does not prevent targeted name
//! squatting; first-instance reservation, owner-only ACLs and peer checks remain
//! the security boundary.

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as LocalListener, UnixStream as LocalStream};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Listener as LocalListener, Stream as LocalStream};

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::Path;
use std::time::Duration;

pub const MAX_REMOTE_REQUEST: usize = isocast_remote::MAX_REQUEST;

pub enum Stream {
    Local(LocalStream),
    Tls(isocast_remote::Duplex),
}

pub struct Listener(LocalListener);

impl Listener {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        LocalListener::bind(path).map(Self)
    }

    pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        self.0.incoming().map(|stream| stream.map(Stream::Local))
    }
}

impl Stream {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        LocalStream::connect(path).map(Self::Local)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Local(stream) => stream.try_clone().map(Self::Local),
            Self::Tls(stream) => stream.try_clone().map(Self::Tls),
        }
    }

    pub fn local(&self) -> Option<&LocalStream> {
        match self {
            Self::Local(stream) => Some(stream),
            Self::Tls(_) => None,
        }
    }

    pub fn peer(&self) -> Option<&isocast_remote::Peer> {
        match self {
            Self::Local(_) => None,
            Self::Tls(stream) => Some(stream.peer()),
        }
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Local(stream) => stream.shutdown(how),
            Self::Tls(stream) => stream.shutdown(how),
        }
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.set_read_timeout(timeout),
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_write_timeout(timeout),
            Self::Tls(stream) => stream.set_write_timeout(timeout),
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_nonblocking(nonblocking),
            Self::Tls(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    pub fn hung_up(&self) -> bool {
        hung_up(self)
    }

    pub fn aborted(&self) -> bool {
        matches!(self, Self::Tls(stream) if stream.aborted())
    }
}

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(stream) => stream.read(bytes),
            Self::Tls(stream) => stream.read(bytes),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(stream) => stream.write(bytes),
            Self::Tls(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Local(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

pub fn hung_up(stream: &Stream) -> bool {
    match stream {
        Stream::Local(stream) => local_hung_up(stream),
        Stream::Tls(stream) => stream.hung_up(),
    }
}

#[cfg(windows)]
fn local_hung_up(stream: &LocalStream) -> bool {
    windows::hung_up(stream)
}

#[cfg(windows)]
pub fn peer_pid(stream: &Stream) -> io::Result<u32> {
    match stream {
        Stream::Local(stream) => windows::peer_pid(stream),
        Stream::Tls(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a TLS peer has no local process id",
        )),
    }
}

#[cfg(windows)]
pub fn readable(stream: &Stream) -> io::Result<bool> {
    match stream {
        Stream::Local(stream) => windows::readable(stream),
        Stream::Tls(stream) => Ok(stream.readable()),
    }
}

/// Whether the peer closed, without consuming any data.
#[cfg(unix)]
fn local_hung_up(stream: &LocalStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut byte = 0u8;
    // SAFETY: a one-byte peek into a local buffer on an open socket.
    let read = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    read == 0
        || (read < 0
            && !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ))
}
