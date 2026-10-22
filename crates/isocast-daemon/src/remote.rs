//! Engine-local remote settings and trust. No remote or plugin caller may
//! change them. The listener's pin persistence callback never re-enters Runtime.
use super::*;
use isocast_protocol::remote::*;
use isocast_remote::{
    Hooks, Identity, PairedClient, Server,
    pairing::Pairing,
    store::{self, Store},
};
use std::io;

#[derive(Default)]
pub(super) struct Runtime {
    inner: Mutex<Inner>,
    pairing: Arc<Pairing>,
}

#[derive(Default)]
struct Inner {
    settings: RemoteSettings,
    server: Option<Server>,
    advertisement: Option<isocast_discovery::Registration>,
    error: Option<String>,
}

fn path() -> PathBuf {
    isocast_protocol::api::setup_path().with_extension("remote.json")
}
fn service() -> String {
    format!(
        "{}.remote.engine",
        app_identifier().unwrap_or_else(|| "dev.isocast.unbundled".into())
    )
}
fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: format!("remote: {error}"),
    }
}
fn identity() -> Result<(Arc<Identity>, RemoteIdentity), RpcError> {
    let path = std::path::absolute(path()).map_err(failed)?;
    let store = Store::lock(&path).map_err(failed)?;
    let account = path.to_string_lossy();
    let (key, public) = store
        .identity(
            callers::machine_name(),
            || {
                native()
                    .secret(&service(), &account)
                    .map_err(io::Error::other)
            },
            |key| {
                native()
                    .set_secret(&service(), &account, key)
                    .map_err(io::Error::other)
            },
        )
        .map_err(failed)?;
    Ok((Arc::new(key), public))
}

impl Runtime {
    pub fn load(&self, daemon: &Arc<Daemon>) {
        let settings = match store::read(&path()) {
            Ok(settings) => settings,
            Err(error) => {
                self.inner.lock().error = Some(error.to_string());
                return;
            }
        };
        let enabled = settings.enabled;
        self.inner.lock().settings = settings;
        if enabled && let Err(error) = self.enable(daemon, RemoteEnableParams::default()) {
            self.inner.lock().error = Some(error.message);
        }
    }
    pub fn status(&self) -> RemoteStatus {
        let inner = self.inner.lock();
        RemoteStatus {
            enabled: inner.settings.enabled,
            running: inner.server.is_some(),
            identity: inner.settings.identity.clone(),
            address: inner
                .server
                .as_ref()
                .map(|server| server.local_addr().to_string()),
            pairing_open: self.pairing.is_open(),
            error: inner.error.clone(),
        }
    }
    pub fn enable(
        &self,
        daemon: &Arc<Daemon>,
        params: RemoteEnableParams,
    ) -> Result<RemoteStatus, RpcError> {
        if params.port == Some(0) {
            return Err(failed("port: expected 1–65535"));
        }
        let (identity, public) = identity()?;
        let mut inner = self.inner.lock();
        let mut settings = store::read(&path()).map_err(failed)?;
        if let Some(bind) = params.bind {
            let _: std::net::IpAddr = bind
                .parse()
                .map_err(|_| failed("bind: expected a numeric IPv4 or IPv6 address"))?;
            settings.bind = Some(bind);
        }
        if params.port.is_some() {
            settings.port = params.port;
        }
        settings.identity = Some(public);
        settings.enabled = true;
        if inner.server.is_some()
            && settings.port == inner.settings.port
            && settings.bind == inner.settings.bind
        {
            inner.settings = settings;
            drop(inner);
            return Ok(self.status());
        }
        // Never hold the store's lock while waiting on the listener's admission
        // lock: a pairing commit holds admission before persisting its pin.
        if let Some(server) = inner.server.take() {
            server.disable();
        }
        inner.advertisement = None;
        let clients = settings
            .peers
            .iter()
            .map(|peer| {
                Ok(PairedClient {
                    spki: store::decode(&peer.spki)?,
                    name: peer.name.clone(),
                })
            })
            .collect::<io::Result<Vec<_>>>()
            .map_err(failed)?;
        let publish = Arc::downgrade(daemon);
        let connected = Arc::downgrade(daemon);
        let networks = Mutex::new((std::time::Instant::now(), prefixes()));
        let server = Server::bind(
            identity,
            clients,
            self.pairing.clone(),
            settings
                .bind
                .as_deref()
                .unwrap_or("0.0.0.0")
                .parse()
                .map_err(failed)?,
            settings.port,
            Hooks {
                on_link: Box::new(move |ip| {
                    let mut networks = networks.lock();
                    if networks.0.elapsed() >= Duration::from_secs(10) {
                        *networks = (std::time::Instant::now(), prefixes());
                    }
                    isocast_protocol::remote::on_link(ip, &networks.1)
                }),
                paired: Box::new(move |spki, name| {
                    let store = Store::lock(&path())?;
                    let mut settings = store.read()?;
                    let client = RemotePeer {
                        id: isocast_remote::fingerprint(spki),
                        name: name.into(),
                        spki: store::encode(spki),
                        paired_at: store::now(),
                        last_seen: Some(store::now()),
                        address: None,
                    };
                    settings.peers.retain(|peer| peer.id != client.id);
                    settings.peers.push(client.clone());
                    store.save(&settings)?;
                    if let Some(daemon) = publish.upgrade() {
                        daemon.publish(Event::RemotePaired { client });
                    }
                    Ok(())
                }),
                connected: Box::new(move |stream| {
                    if let Some(daemon) = connected.upgrade() {
                        let peer = audit_peer(stream.peer());
                        if let Ok(store) = Store::lock(&path())
                            && let Ok(mut settings) = store.read()
                            && let Some(client) = settings
                                .peers
                                .iter_mut()
                                .find(|client| client.id == peer.client_id)
                            && client
                                .last_seen
                                .is_none_or(|last| store::now().saturating_sub(last) >= 60)
                        {
                            client.last_seen = Some(store::now());
                            let _ = store.save(&settings);
                        }
                        daemon.publish(Event::RemoteClientConnected { peer: peer.clone() });
                        daemon.contain("remote API connection", || {
                            let caller = isocast_protocol::caller::Caller {
                                origin: isocast_protocol::caller::Origin::Remote {
                                    client_id: stream.peer().client_id.clone(),
                                    name: stream.peer().name.clone(),
                                    addr: stream.peer().addr,
                                },
                            };
                            let _ = handle(UnixStream::Tls(stream), &daemon, None, caller);
                        });
                        daemon.publish(Event::RemoteClientDisconnected { peer });
                    }
                }),
            },
        )
        .map_err(failed)?;
        settings.port = Some(server.local_addr().port());
        // Retain the newest peers; pairing commits may have updated the file.
        {
            let store = Store::lock(&path()).map_err(failed)?;
            settings.peers = store.read().map_err(failed)?.peers;
            store.save(&settings).map_err(failed)?;
        }
        inner.error = None;
        if !server.local_addr().ip().is_loopback() {
            let identity = settings
                .identity
                .as_ref()
                .expect("identity was loaded before binding");
            match isocast_discovery::register(
                &identity.name,
                server.local_addr().port(),
                &isocast_discovery::Txt {
                    id: identity.id.clone(),
                    proto: "1".into(),
                    name: identity.name.clone(),
                },
            ) {
                Ok(registration) => inner.advertisement = Some(registration),
                Err(error) => {
                    inner.error = Some(format!(
                        "listening by address, but discovery is unavailable: {error}; allow Local Network access for Isocast on this engine"
                    ))
                }
            }
        }
        inner.settings = settings;
        inner.server = Some(server);
        drop(inner);
        Ok(self.status())
    }
    pub fn disable(&self, daemon: &Daemon) -> Result<RemoteStatus, RpcError> {
        let mut inner = self.inner.lock();
        if let Some(server) = inner.server.take() {
            server.disable();
        }
        inner.advertisement = None;
        self.pairing.close();
        inner.settings.enabled = false;
        let store = Store::lock(&path()).map_err(failed)?;
        let mut settings = store.read().map_err(failed)?;
        settings.enabled = false;
        store.save(&settings).map_err(failed)?;
        drop(store);
        drop(inner);
        daemon.publish(Event::RemoteDisabled);
        Ok(self.status())
    }
    pub fn begin(&self) -> Result<RemotePairWindow, RpcError> {
        let inner = self.inner.lock();
        let server = inner
            .server
            .as_ref()
            .ok_or_else(|| failed("enable remote control before opening a pairing window"))?;
        let identity = inner
            .settings
            .identity
            .clone()
            .ok_or_else(|| failed("engine identity unavailable"))?;
        let code = self.pairing.begin().map_err(failed)?;
        Ok(RemotePairWindow {
            code: code.to_string(),
            expires_in_seconds: 120,
            identity,
            address: server.local_addr().to_string(),
        })
    }
    pub fn clients(&self) -> Result<Vec<RemotePeer>, RpcError> {
        Ok(store::read(&path()).map_err(failed)?.peers)
    }
    pub fn revoke(&self, daemon: &Daemon, id: &str) -> Result<Vec<RemotePeer>, RpcError> {
        let inner = self.inner.lock();
        let persist = || -> io::Result<Vec<RemotePeer>> {
            let store = Store::lock(&path())?;
            let mut settings = store.read()?;
            let before = settings.peers.len();
            settings.peers.retain(|client| client.id != id);
            if before == settings.peers.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "client_id: no paired client has that id",
                ));
            }
            store.save(&settings)?;
            Ok(settings.peers)
        };
        let peers = match &inner.server {
            Some(server) => server.revoke_with(id, persist),
            None => persist(),
        }
        .map_err(failed)?;
        drop(inner);
        daemon.publish(Event::RemoteRevoked {
            client_id: id.into(),
        });
        Ok(peers)
    }
}

pub(super) fn audit_peer(peer: &isocast_remote::Peer) -> RemoteAuditPeer {
    RemoteAuditPeer {
        transport: "tls".into(),
        client_id: peer.client_id.clone(),
        name: peer.name.clone(),
        addr: peer.addr.to_string(),
    }
}

fn prefixes() -> Vec<(std::net::IpAddr, u8)> {
    #[cfg(target_os = "macos")]
    {
        isocast_platform_macos::network::prefixes()
    }
    #[cfg(windows)]
    {
        isocast_platform_windows::network::prefixes()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Vec::new()
    }
}
