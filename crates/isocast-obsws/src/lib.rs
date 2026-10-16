//! An obs-websocket v5 server (rpcVersion 1) on top of isocastd's method
//! table (#71), so tools written for OBS (Stream Deck's and Touch Portal's
//! OBS actions, Streamer.bot, Companion, bots, overlays, scripts) drive
//! Isocast unchanged.
//!
//! It is a thin client: every request becomes calls of the same methods
//! the CLI and MCP use ([`Engine::call`]), and every OBS event comes from
//! the engine's own events ([`Engine::watch`]) or from comparing the live
//! setup before and after a `setup_changed`. OBS names and uuids map to
//! Isocast ids deterministically (see `world`). Requests it doesn't map
//! answer `UnknownRequestType` (204) with a comment saying Isocast doesn't
//! support them.
//!
//! The protocol: <https://github.com/obsproject/obs-websocket/blob/master/docs/generated/protocol.md>.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use isocast_protocol::api::{EventKinds, Method, RpcError};
use serde_json::Value;

mod events;
mod requests;
mod session;
mod world;

pub use requests::SUPPORTED;

/// The obs-websocket version this server speaks (the newest whose
/// requests it maps: `CreateRecordChapter` is 5.5).
pub const OBS_WEBSOCKET_VERSION: &str = "5.5.0";
/// The OBS version it reports: one that has every request it maps.
pub const OBS_VERSION: &str = "30.2.0";
/// The protocol's rpcVersion.
pub const RPC_VERSION: u32 = 1;
/// Clients connected at once; more are refused with 503 (#74).
pub const MAX_CLIENTS: u32 = 32;

/// isocastd, as the server sees it.
pub trait Engine: Send + Sync + 'static {
    /// Call the method table. `peer` names the client, for the log.
    fn call(&self, method: Method, peer: &str) -> Result<Value, RpcError>;
    /// Watch events of `kinds`: each a JSON-RPC `event` notification line,
    /// as `watch` sends them. The channel closes if the watcher falls too
    /// far behind; dropping the [`Watching`] stops it.
    fn watch(&self, kinds: EventKinds) -> Watching;
}

/// Events coming from [`Engine::watch`].
pub struct Watching {
    pub events: mpsc::Receiver<Arc<[u8]>>,
    /// Stops the watch when dropped.
    pub guard: Box<dyn Send>,
}

/// How to run the server.
#[derive(Clone)]
pub struct Config {
    pub addr: SocketAddr,
    /// Clients must authenticate with this, if given.
    pub password: Option<String>,
    /// Isocast's version, for `GetVersion`'s platformDescription.
    pub isocast_version: String,
}

/// A running server; stops (disconnecting its clients) when dropped.
pub struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<thread::JoinHandle<()>>,
}

pub(crate) struct Shared {
    pub stop: AtomicBool,
    pub clients: AtomicU32,
    pub engine: Arc<dyn Engine>,
    pub password: Option<String>,
    /// One salt per server, as obs-websocket keeps one per run.
    pub salt: String,
    pub isocast_version: String,
    /// Each identified session's inbox for `BroadcastCustomEvent`.
    pub custom: parking_lot::Mutex<Vec<mpsc::Sender<Value>>>,
}

impl Server {
    /// Listen on `config.addr` and serve clients until dropped. Fails if
    /// the address can't be bound (the port is taken).
    pub fn start(config: Config, engine: Arc<dyn Engine>) -> io::Result<Self> {
        let listener = TcpListener::bind(config.addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            clients: AtomicU32::new(0),
            engine,
            password: config.password.filter(|p| !p.is_empty()),
            salt: random_base64(32)?,
            isocast_version: config.isocast_version,
            custom: parking_lot::Mutex::new(Vec::new()),
        });
        let accept = thread::Builder::new().name("obs-websocket".into()).spawn({
            let shared = shared.clone();
            move || accept_loop(listener, shared)
        })?;
        Ok(Self {
            addr,
            shared,
            accept: Some(accept),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Clients connected now.
    pub fn clients(&self) -> u32 {
        self.shared.clients.load(Ordering::SeqCst)
    }

    pub fn authentication(&self) -> bool {
        self.shared.password.is_some()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        // Sessions notice within one poll, say goodbye and close.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.shared.clients.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if shared.clients.load(Ordering::SeqCst) >= MAX_CLIENTS {
                    // Each client is a thread; a flood mustn't exhaust them.
                    use std::io::Write;
                    let _ = stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\
                          Content-Length: 0\r\n\r\n",
                    );
                    continue;
                }
                let shared = shared.clone();
                shared.clients.fetch_add(1, Ordering::SeqCst);
                let spawned = thread::Builder::new()
                    .name("obs-websocket client".into())
                    .spawn({
                        let shared = shared.clone();
                        move || {
                            struct Count(Arc<Shared>);
                            impl Drop for Count {
                                fn drop(&mut self) {
                                    self.0.clients.fetch_sub(1, Ordering::SeqCst);
                                }
                            }
                            let _count = Count(shared.clone());
                            session::run(stream, peer, &shared);
                        }
                    });
                if spawned.is_err() {
                    shared.clients.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// `bytes` random bytes, base64-encoded.
pub(crate) fn random_base64(bytes: usize) -> io::Result<String> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(random_bytes(bytes)?))
}

pub(crate) fn random_bytes(count: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = vec![0u8; count];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut out)?;
    Ok(out)
}

/// A password to show once and type into an OBS client: 16 letters and
/// digits (about 95 bits), without look-alikes.
pub fn generate_password() -> io::Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::new();
    while out.len() < 16 {
        for byte in random_bytes(32)? {
            // Rejection sampling keeps every character equally likely.
            let limit = 256 - 256 % ALPHABET.len();
            if (byte as usize) < limit && out.len() < 16 {
                out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    Ok(out)
}

/// What a client must send to authenticate: base64(SHA-256(base64(SHA-256(
/// password + salt)) + challenge)).
pub fn authentication(password: &str, salt: &str, challenge: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let b64 = &base64::engine::general_purpose::STANDARD;
    let secret = b64.encode(Sha256::digest(format!("{password}{salt}")));
    b64.encode(Sha256::digest(format!("{secret}{challenge}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_matches_the_protocol() {
        // The protocol's example inputs; the expected value computed
        // independently (Python's hashlib and base64).
        let salt = "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=";
        let challenge = "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=";
        assert_eq!(
            authentication("supersecretpassword", salt, challenge),
            "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4="
        );
    }

    #[test]
    fn generated_passwords_are_long_and_plain() {
        let a = generate_password().unwrap();
        let b = generate_password().unwrap();
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }
}
