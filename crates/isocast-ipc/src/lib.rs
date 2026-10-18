//! The daemon's local byte-stream transport. Unix retains the standard
//! library's types and behavior; Windows uses authenticated local named pipes.
//!
//! Windows pipe names include the process user's binary SID and the normalized
//! logical path. This namespace replaced path-only names: restart existing
//! Windows development daemons when upgrading; there is no old-name fallback.
//! A SID is not secret. Namespace isolation does not prevent targeted name
//! squatting; first-instance reservation, owner-only ACLs and peer checks remain
//! the security boundary.

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Incoming, Listener, Stream, hung_up, peer_pid};

/// Whether the peer closed, without consuming any data.
#[cfg(unix)]
pub fn hung_up(stream: &Stream) -> bool {
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
