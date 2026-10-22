use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::{Duplex, Identity, Peer, Pins, pairing::Pairing, tls};

const HANDSHAKES: usize = 8;
const TOTAL: usize = 64;
const PER_CLIENT: usize = 16;
const RATE_IPS: usize = 1024;
const RATE_WINDOW: Duration = Duration::from_secs(10);
// Control clients (especially CLI/MCP) reconnect for individual calls. Allow
// short legitimate bursts; concurrent handshakes and live clients remain capped.
const RATE_ATTEMPTS: u32 = 128;

#[derive(Clone)]
pub struct PairedClient {
    pub spki: Vec<u8>,
    pub name: String,
}

pub type PersistPair = dyn Fn(&[u8], &str) -> io::Result<()> + Send + Sync;

pub struct Hooks {
    /// True only for a peer inside a prefix of a local network interface.
    /// Supplements the built-in private/link-local/Tailscale ranges with
    /// on-link IPv6 global addresses; discovery is not proof of being local.
    pub on_link: Box<dyn Fn(IpAddr) -> bool + Send + Sync>,
    /// Persist the public key and name atomically, without calling Server.
    pub paired: Box<PersistPair>,
    /// Serve ordinary requests. Returning drops the admission slot. No daemon
    /// work, JSON parsing or control handler is run on the handshake pool.
    pub connected: Box<dyn Fn(Duplex) + Send + Sync>,
}

pub struct Server {
    state: Arc<Mutex<State>>,
    pairing: Arc<Pairing>,
    pins: Arc<Pins>,
    addr: SocketAddr,
    accept_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct State {
    enabled: bool,
    handshakes: usize,
    next: u64,
    peers: HashMap<String, PairedClient>,
    active: HashMap<u64, Active>,
    rates: HashMap<IpAddr, (Instant, u32)>,
}

struct Active {
    socket: TcpStream,
    client: Option<String>,
    close: Option<crate::duplex::CloseHandle>,
}

struct Admission {
    state: Arc<Mutex<State>>,
    id: u64,
    handshake: bool,
}

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.active.remove(&self.id);
        if self.handshake {
            state.handshakes -= 1;
        }
    }
}

impl Server {
    /// `port = None` tries the default and following ports for another user.
    /// `Some(0)` selects an OS-assigned port for isolated runtime checks.
    pub fn bind(
        identity: Arc<Identity>,
        peers: Vec<PairedClient>,
        pairing: Arc<Pairing>,
        bind: IpAddr,
        port: Option<u16>,
        hooks: Hooks,
    ) -> io::Result<Self> {
        let listener = match port {
            Some(port) => TcpListener::bind((bind, port))?,
            None => {
                let mut port = crate::DEFAULT_PORT;
                loop {
                    match TcpListener::bind((bind, port)) {
                        Ok(listener) => break listener,
                        Err(error)
                            if error.kind() == io::ErrorKind::AddrInUse
                                && port < crate::DEFAULT_PORT + 100 =>
                        {
                            port += 1
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        };
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let pins = Arc::new(Pins::default());
        for peer in &peers {
            pins.insert(peer.spki.clone())?;
        }
        let config = tls::server_config(&identity, pins.clone(), pairing.clone())?;
        let state = Arc::new(Mutex::new(State {
            enabled: true,
            handshakes: 0,
            next: 0,
            peers: peers
                .into_iter()
                .map(|peer| (tls::fingerprint(&peer.spki), peer))
                .collect(),
            active: HashMap::new(),
            rates: HashMap::new(),
        }));
        let server = Self {
            state: state.clone(),
            pins: pins.clone(),
            pairing: pairing.clone(),
            addr,
            accept_thread: Mutex::new(None),
        };
        let hooks = Arc::new(hooks);
        let (send, receive) = mpsc::sync_channel::<(TcpStream, Admission, Instant)>(HANDSHAKES);
        let receive = Arc::new(Mutex::new(receive));
        for _ in 0..HANDSHAKES {
            let receive = receive.clone();
            let state = state.clone();
            let config = config.clone();
            let identity = identity.clone();
            let pins = pins.clone();
            let pairing = pairing.clone();
            let hooks = hooks.clone();
            std::thread::Builder::new()
                .name("remote handshake".into())
                .spawn(move || {
                    loop {
                        let job = receive.lock().unwrap().recv();
                        let Ok((socket, admission, deadline)) = job else {
                            break;
                        };
                        let _ = accept(
                            socket, admission, deadline, &state, &config, &identity, &pins,
                            &pairing, &hooks,
                        );
                    }
                })?;
        }
        let admission_hooks = hooks.clone();
        let accept_thread = std::thread::Builder::new()
            .name("remote listener".into())
            .spawn(move || {
                loop {
                    if !state.lock().unwrap().enabled {
                        break;
                    }
                    match listener.accept() {
                        Ok((socket, addr)) => {
                            if !local_address(addr.ip()) && !(admission_hooks.on_link)(addr.ip()) {
                                continue;
                            }
                            if let Ok(Some(admission)) = admit(&state, &socket, addr.ip()) {
                                // Never block accepting on pool saturation.
                                let _ = send.try_send((
                                    socket,
                                    admission,
                                    Instant::now() + Duration::from_secs(2),
                                ));
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20))
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })?;
        *server.accept_thread.lock().unwrap() = Some(accept_thread);
        Ok(server)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Revoke an in-memory identity. Persistent servers use revoke_with so
    /// removal from disk and from the admission table are one transaction.
    pub fn revoke(&self, id: &str) {
        let _ = self.revoke_with(id, || Ok(()));
    }

    /// Serialize persistence with pairing commits, admission and revocation.
    /// The callback must not re-enter Server and must not already hold its
    /// store lock (pairing takes the admission lock before that store lock).
    pub fn revoke_with<T>(
        &self,
        id: &str,
        persist: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let mut state = self.state.lock().unwrap();
        let result = persist()?;
        if let Some(peer) = state.peers.remove(id) {
            self.pins.remove(&peer.spki);
        }
        for active in state.active.values() {
            if active.client.as_deref() == Some(id) {
                active.close();
            }
        }
        Ok(result)
    }

    pub fn disable(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.enabled = false;
            for active in state.active.values() {
                active.close();
            }
        }
        self.pairing.close();
        // Release the listening port before disable returns. A subsequent
        // enable may bind the same port immediately, even on another interface.
        if let Some(thread) = self.accept_thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

impl Active {
    fn close(&self) {
        if let Some(close) = &self.close {
            close.close();
        }
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.disable();
    }
}

fn admit(
    state: &Arc<Mutex<State>>,
    socket: &TcpStream,
    ip: IpAddr,
) -> io::Result<Option<Admission>> {
    let mut inner = state.lock().unwrap();
    if !inner.enabled || inner.handshakes >= HANDSHAKES || inner.active.len() >= TOTAL {
        return Ok(None);
    }
    let ip = match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    };
    let now = Instant::now();
    inner
        .rates
        .retain(|_, (start, _)| now.duration_since(*start) < RATE_WINDOW);
    if !inner.rates.contains_key(&ip) && inner.rates.len() >= RATE_IPS {
        return Ok(None);
    }
    let (_, count) = inner.rates.entry(ip).or_insert((now, 0));
    if *count >= RATE_ATTEMPTS {
        return Ok(None);
    }
    *count += 1;
    let socket = socket.try_clone()?;
    let id = inner.next;
    inner.next += 1;
    inner.handshakes += 1;
    inner.active.insert(
        id,
        Active {
            socket,
            client: None,
            close: None,
        },
    );
    Ok(Some(Admission {
        state: state.clone(),
        id,
        handshake: true,
    }))
}

#[allow(clippy::too_many_arguments)]
fn accept(
    socket: TcpStream,
    mut admission: Admission,
    deadline: Instant,
    state: &Arc<Mutex<State>>,
    config: &Arc<rustls::ServerConfig>,
    identity: &Arc<Identity>,
    pins: &Arc<Pins>,
    pairing: &Arc<Pairing>,
    hooks: &Arc<Hooks>,
) -> io::Result<()> {
    let addr = socket.peer_addr()?;
    let connection = rustls::ServerConnection::new(config.clone()).map_err(io::Error::other)?;
    let (socket, connection) = tls::handshake(socket, connection.into(), deadline)?;
    let spki = connection
        .peer_certificates()
        .and_then(|keys| keys.first())
        .ok_or_else(|| io::Error::other("missing client key"))?
        .as_ref()
        .to_vec();
    let client_id = tls::fingerprint(&spki);
    let mut inner = state.lock().unwrap();
    if !inner.enabled {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let paired = inner
        .peers
        .get(&client_id)
        .cloned()
        .filter(|peer| peer.spki == spki && pins.contains(&spki));
    if paired.is_some()
        && inner
            .active
            .values()
            .filter(|active| active.client.as_deref() == Some(&client_id))
            .count()
            >= PER_CLIENT
    {
        return Err(io::Error::other("remote connection limit for this client"));
    }
    let name = paired
        .as_ref()
        .map(|peer| peer.name.clone())
        .unwrap_or_default();
    let stream = Duplex::start(
        socket,
        connection,
        Peer {
            client_id: client_id.clone(),
            name,
            addr,
            spki: spki.clone(),
        },
    )?;
    let active = inner.active.get_mut(&admission.id).unwrap();
    active.client = Some(client_id.clone());
    active.close = Some(stream.close_handle());
    inner.handshakes -= 1;
    admission.handshake = false;
    drop(inner);
    // Claim outside the admission lock: pairing's final persistence callback
    // takes it while holding the pairing lock (close and commit serialize).
    let attempt = if paired.is_none() {
        Some(pairing.claim()?)
    } else {
        None
    };
    let state = state.clone();
    let hooks = hooks.clone();
    let identity = identity.clone();
    let pins = pins.clone();
    let pairing = pairing.clone();
    std::thread::Builder::new()
        .name("remote client".into())
        .spawn(move || {
            let _admission = admission;
            // A known client may have forgotten its engine pin. It must be
            // able to perform the same PAKE again, never an unauthenticated
            // repin. Inspect only during an explicitly opened local window.
            let attempt = if attempt.is_none() && pairing.is_open() {
                match crate::pairing::requested(&stream) {
                    Ok(true) => match pairing.claim() {
                        Ok(attempt) => Some(attempt),
                        Err(_) => return,
                    },
                    Ok(false) => None,
                    Err(_) => return,
                }
            } else {
                attempt
            };
            if let Some(attempt) = attempt {
                let mut stream = stream;
                let close = stream.close_handle();
                let _ = attempt.finish(&mut stream, &identity, |spki, name| {
                    let mut inner = state.lock().unwrap();
                    if !inner.enabled || close.aborted() {
                        return Err(io::ErrorKind::PermissionDenied.into());
                    }
                    (hooks.paired)(spki, name)?;
                    pins.insert(spki.to_vec())?;
                    inner.peers.insert(
                        client_id,
                        PairedClient {
                            spki: spki.to_vec(),
                            name: name.into(),
                        },
                    );
                    Ok(())
                });
            } else if !stream.aborted() {
                (hooks.connected)(stream);
            }
        })?;
    Ok(())
}

pub fn local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || (u32::from(ip) & 0xffc00000 == 0x64400000)
        }
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(|ip| local_address(ip.into()))
            .unwrap_or_else(|| {
                ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
            }),
    }
}
