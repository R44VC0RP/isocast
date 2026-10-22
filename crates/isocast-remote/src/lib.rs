//! Blocking, mutually authenticated remote transport. The TLS connection is
//! owned by a driver, never locked by a caller doing a blocking read or write.

mod duplex;
mod listener;
pub mod pairing;
pub mod store;
mod tls;

pub use duplex::{Duplex, Peer};
pub use listener::{Hooks, PairedClient, Server, local_address};
pub use rustls::ClientConfig;
pub use tls::{
    Identity, Pins, client_config, connect, fingerprint, pairing_client_config, server_config,
};

pub const DEFAULT_PORT: u16 = 47470;
pub const MAX_REQUEST: usize = 1024 * 1024;
pub const ALPN: &[u8] = b"isocast/1";
