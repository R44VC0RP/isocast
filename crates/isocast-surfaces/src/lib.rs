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
//! the transport and release contracts.
//!
//! Windows uses same-session named DX12 allocations and one-time offers over
//! authenticated named pipes. The client checks the adapter and acknowledges
//! a frame only after its rendering queue's fence completes.
//!
//! Linux shares memory, not GPU memory: each surface is a sealed memfd the
//! daemon reads its drawn picture back into, handed over once per ring as
//! descriptors (`SCM_RIGHTS`) through a local socket in the abstract
//! namespace; the client maps it read-only. No H.264, and no GPU needed.
//!
//! On other systems everything here returns an error saying so.

use std::fmt;

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use mac::{Exporter, Surface, fetch, host_now, retained_surfaces};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Exporter, Surface, fetch, host_now};
#[cfg(all(windows, feature = "wgpu"))]
pub use windows::{check_render_queue, dx12_adapter, dx12_adapter_luid};
#[cfg(all(windows, feature = "wgpu"))]
pub use windows::{retire, retirement_failed};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{Exporter, Surface, fetch, host_now};

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod other;

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub use other::{Exporter, Surface, fetch, host_now};

#[cfg(target_os = "macos")]
pub const TRANSPORT: &str = "iosurface-mach";
#[cfg(windows)]
pub const TRANSPORT: &str = "d3d12-named-pipe";
#[cfg(target_os = "linux")]
pub const TRANSPORT: &str = "memfd-unix";
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub const TRANSPORT: &str = "unsupported";

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

#[cfg(windows)]
fn fill_random(bytes: &mut [u8]) {
    use ::windows::Win32::Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
    };
    unsafe { BCryptGenRandom(None, bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .expect("system random generator");
}

#[cfg(not(any(unix, windows)))]
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
