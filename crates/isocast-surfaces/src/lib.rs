//! GPU surfaces `isocastd` shares with its clients without copying: a
//! monitor's picture is drawn by the daemon's GPU into a small ring of
//! surfaces, and the client's GPU shows them in place.
//!
//! On macOS a surface is an `IOSurface`. A Unix socket can't carry one, so
//! the surfaces of a ring are handed over once, as Mach ports
//! (`IOSurfaceCreateMachPort`), through a Mach service the daemon registers
//! with the bootstrap server under a name of its own ([`Exporter`]); the
//! client is told that name and a one-time token over the control socket
//! and fetches the ring with [`fetch`]. After that only slot numbers travel
//! (see `isocast_protocol::api::shared`). docs/shared-frames.md explains
//! the choice and the Windows design.
//!
//! On other systems everything here returns an error saying so.

use std::fmt;

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use mac::{Exporter, Surface, fetch, host_now, retained_surfaces};

#[cfg(not(target_os = "macos"))]
mod other;

#[cfg(not(target_os = "macos"))]
pub use other::{Exporter, Surface, fetch, host_now};

/// The most surfaces a ring may have.
pub const MAX_SLOTS: usize = 8;

/// A secret that lets one client fetch one ring's surfaces, once. Shown as
/// 32 hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Token(pub [u8; 16]);

impl Token {
    /// A random token.
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        fill_random(&mut bytes);
        Self(bytes)
    }

    /// Read a token from its 32 hex digits.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("token: {text:?} is not 32 hex digits");
        }
        let mut bytes = [0u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(…)")
    }
}

#[cfg(unix)]
fn fill_random(bytes: &mut [u8]) {
    use std::io::Read;
    // The kernel's generator; never fails short on macOS or Linux.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(bytes))
        .expect("could not read /dev/urandom");
}

#[cfg(not(unix))]
fn fill_random(bytes: &mut [u8]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    for chunk in bytes.chunks_mut(8) {
        let value = RandomState::new().build_hasher().finish().to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::Token;

    #[test]
    fn tokens_read_back_as_shown() {
        let token = Token::random();
        assert_eq!(Token::parse(&token.to_string()).unwrap(), token);
        assert_ne!(Token::random(), token);
        let err = Token::parse("xyz").unwrap_err().to_string();
        assert!(err.contains("32 hex digits"), "{err}");
    }
}
