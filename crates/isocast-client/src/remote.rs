use anyhow::{Context, Result, bail};
use isocast_protocol::remote::{RemoteIdentity, RemotePeer};
use isocast_remote::{
    Identity, Pins,
    store::{self, Store},
};
use std::{
    io,
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// A connection target, independent of whether a local daemon is installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Engine {
    #[default]
    Local,
    Remote {
        target: String,
    },
}

static DEFAULT_ENGINE: OnceLock<Engine> = OnceLock::new();

impl Engine {
    pub fn parse(target: &str) -> Result<Self> {
        if target == "local" {
            return Ok(Self::Local);
        }
        if target.trim().is_empty() {
            bail!("engine: expected local, a paired name/id, or host[:port]");
        }
        Ok(Self::Remote {
            target: target.into(),
        })
    }
    pub fn selected() -> Result<Self> {
        if let Some(engine) = DEFAULT_ENGINE.get() {
            return Ok(engine.clone());
        }
        std::env::var("ISOCAST_ENGINE")
            .ok()
            .map(|target| Self::parse(&target))
            .transpose()
            .map(|engine| engine.unwrap_or_default())
    }
    /// Configure CLI/MCP once, before its worker threads start. No mutable
    /// environment variables, and windows can use explicit Engine handles.
    pub fn select(engine: Self) -> Result<()> {
        DEFAULT_ENGINE
            .set(engine)
            .map_err(|_| anyhow::anyhow!("engine target was already selected"))
    }
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
    pub fn connect(&self) -> Result<Option<crate::Client>> {
        match self {
            Self::Local => crate::Client::connect_local(),
            Self::Remote { target } => connect(target).map(Some),
        }
    }
    pub fn connect_or_launch(&self) -> Result<crate::Client> {
        match self {
            Self::Local => crate::Client::connect_local_or_launch(),
            Self::Remote { target } => connect(target),
        }
    }
}

pub fn path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISOCAST_REMOTE_CLIENT") {
        return path.into();
    }
    let executable = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "client".into());
    isocast_protocol::api::support_folder()
        .join("remote")
        .join(format!("{executable}.json"))
}

fn key_read(account: &str) -> io::Result<Option<String>> {
    isocast_platform_credentials::secret("dev.isocast.remote.client", account)
        .map_err(io::Error::other)
}

fn key_write(account: &str, value: &str) -> io::Result<()> {
    isocast_platform_credentials::set_secret("dev.isocast.remote.client", account, value)
        .map_err(io::Error::other)
}

fn load_identity() -> Result<(Identity, RemoteIdentity)> {
    let path = std::path::absolute(path())?;
    let account = path.to_string_lossy();
    let store = Store::lock(&path)?;
    store
        .identity(
            &store::machine_name(),
            || key_read(&account),
            |key| key_write(&account, key),
        )
        .context(
            "could not load the client identity from its OS credential store (no key was replaced)",
        )
}

pub fn identity() -> Result<RemoteIdentity> {
    Ok(load_identity()?.1)
}
pub fn engines() -> Result<Vec<RemotePeer>> {
    Ok(store::read(&path())?.peers)
}

fn endpoint(target: &str) -> Result<String> {
    let target = target.strip_prefix("tls://").unwrap_or(target);
    if target.is_empty()
        || target
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '?' | '#'))
    {
        bail!("engine address: expected host[:port], without a URL path or credentials");
    }
    let address = if let Ok(ip) = target.parse::<std::net::IpAddr>() {
        SocketAddr::new(ip, isocast_remote::DEFAULT_PORT).to_string()
    } else if let Ok(address) = target.parse::<SocketAddr>() {
        address.to_string()
    } else if let Some((host, port)) = target.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        format!("{host}:{port}")
    } else {
        format!("{target}:{}", isocast_remote::DEFAULT_PORT)
    };
    Ok(address.to_ascii_lowercase())
}

fn same_address(left: &str, right: &str) -> bool {
    endpoint(left)
        .ok()
        .zip(endpoint(right).ok())
        .is_some_and(|(left, right)| left == right)
}

fn address(target: &str) -> Result<Vec<SocketAddr>> {
    let addresses: Vec<_> = endpoint(target)?
        .to_socket_addrs()
        .with_context(|| format!("engine address {target:?} could not be resolved"))?
        .collect();
    if addresses.is_empty() {
        bail!("engine address resolved to no addresses");
    }
    Ok(addresses)
}

fn transport(
    target: &str,
    config: Arc<isocast_remote::ClientConfig>,
) -> Result<isocast_remote::Duplex> {
    let mut last = None;
    for address in address(target)?.into_iter().take(8) {
        match isocast_remote::connect(address, config.clone()) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("engine has no address")))
    .with_context(|| {
        format!(
            "could not connect to engine {target:?}; check its address, network access and pairing"
        )
    })
}

fn connect(target: &str) -> Result<crate::Client> {
    let peers = engines()?;
    let matches: Vec<_> = peers
        .iter()
        .filter(|peer| {
            peer.id == target
                || peer.name == target
                || peer
                    .address
                    .as_deref()
                    .is_some_and(|address| same_address(address, target))
        })
        .collect();
    let peer = match matches.as_slice() {
        [peer] => *peer,
        [] => bail!(
            "engine {target:?} is not paired; run `isocast remote pair {target} --code-stdin`"
        ),
        _ => bail!("engine name {target:?} is ambiguous; use its full id"),
    };
    let (identity, _) = load_identity()?;
    let pins = Arc::new(Pins::default());
    pins.insert(store::decode(&peer.spki)?)?;
    let address = peer
        .address
        .as_deref()
        .context("paired engine has no address")?;
    let stream = transport(address, isocast_remote::client_config(&identity, pins)?)?;
    let mut client = crate::Client::new(isocast_ipc::Stream::Tls(stream))?;
    use isocast_protocol::hello::{
        BuildIdentity, Compatibility, Hello, PROTOCOL, Provenance, compatibility,
    };
    let hello: Hello = client.call(isocast_protocol::api::Method::Hello)?;
    if hello.protocol != PROTOCOL || hello.engine_id.as_deref() != Some(&peer.id) {
        bail!("the engine's hello does not match its authenticated identity/protocol");
    }
    let own = BuildIdentity {
        version: env!("CARGO_PKG_VERSION").into(),
        commit: env!("ISOCAST_CLIENT_COMMIT").into(),
        provenance: Provenance::from_build(Some(env!("ISOCAST_CLIENT_PROVENANCE"))),
    };
    match compatibility(&own, &hello.build) {
        Compatibility::Same => {}
        Compatibility::CommitDiffers(message) => {
            isocast_diagnostics::diagnostic!("warn"; "{message}")
        }
        Compatibility::Mismatch(message) => client.compatibility_error = Some(message),
    }
    client.hello = Some(hello);
    Ok(client)
}

pub fn pair(target: &str, code: &str, name: Option<&str>) -> Result<RemotePeer> {
    let (identity, public) = load_identity()?;
    let mut stream = transport(target, isocast_remote::pairing_client_config(&identity)?)?;
    if engines()?.iter().any(|peer| {
        peer.address
            .as_deref()
            .is_some_and(|address| same_address(address, target))
            && peer.id != stream.peer().client_id
    }) {
        bail!(
            "this engine's identity changed; explicitly forget its old identity before pairing again"
        );
    }
    let spki = isocast_remote::pairing::finish(
        &mut stream,
        &identity,
        name.unwrap_or(&public.name),
        code,
    )?;
    let mut peer = RemotePeer {
        id: isocast_remote::fingerprint(&spki),
        name: target.trim_start_matches("tls://").into(),
        spki: store::encode(&spki),
        paired_at: store::now(),
        last_seen: None,
        address: Some(format!("tls://{}", endpoint(target)?)),
    };
    drop(stream);
    // Persist confirmed trust before an optional second connection. Losing the
    // link while fetching the human name must not lose a successful pairing.
    save_peer(&peer)?;
    // Human names are learned only from an authenticated engine, never DNS-SD.
    let pins = Arc::new(Pins::default());
    pins.insert(spki)?;
    let metadata = || -> Result<isocast_protocol::remote::RemoteStatus> {
        let stream = transport(target, isocast_remote::client_config(&identity, pins)?)?;
        let mut client = crate::Client::new(isocast_ipc::Stream::Tls(stream))?;
        client.call(isocast_protocol::api::Method::RemoteStatus)
    };
    if let Ok(status) = metadata()
        && let Some(public) = status.identity
    {
        if public.id != peer.id {
            bail!("engine status identity does not match its TLS key");
        }
        peer.name = public.name;
        save_peer(&peer)?;
    }
    Ok(peer)
}

fn save_peer(peer: &RemotePeer) -> Result<()> {
    let store = Store::lock(&path())?;
    let mut settings = store.read()?;
    if settings.peers.iter().any(|existing| {
        existing
            .address
            .as_deref()
            .zip(peer.address.as_deref())
            .is_some_and(|(left, right)| same_address(left, right))
            && existing.id != peer.id
    }) {
        bail!(
            "this engine's identity changed; explicitly forget its old identity before pairing again"
        );
    }
    settings.peers.retain(|existing| existing.id != peer.id);
    settings.peers.push(peer.clone());
    store.save(&settings)?;
    Ok(())
}

pub fn forget(id: &str) -> Result<()> {
    let store = Store::lock(&path())?;
    let mut settings = store.read()?;
    let before = settings.peers.len();
    settings.peers.retain(|peer| peer.id != id);
    if settings.peers.len() == before {
        bail!("engine id: no paired engine {id:?}");
    }
    store.save(&settings)?;
    Ok(())
}
