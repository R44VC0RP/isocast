use std::collections::HashSet;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ring::signature::KeyPair;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{
    CertificateDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme,
};

use crate::{ALPN, Duplex, Peer};

// RFC 8410's canonical SubjectPublicKeyInfo prefix for a 32-byte Ed25519 key.
const ED25519_SPKI: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// No Debug implementation: neither private keys nor pairing secrets belong in logs.
pub struct Identity {
    key: Arc<rustls::sign::CertifiedKey>,
    spki: Vec<u8>,
}

impl Identity {
    /// The caller stores these bytes in its platform credential store, not a file.
    pub fn generate() -> io::Result<zeroize::Zeroizing<Vec<u8>>> {
        let key = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| io::Error::other("could not generate an Ed25519 identity"))?;
        Ok(zeroize::Zeroizing::new(key.as_ref().to_vec()))
    }

    pub fn from_pkcs8(bytes: &[u8]) -> io::Result<Self> {
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(bytes)
            .map_err(|_| io::Error::other("invalid Ed25519 identity"))?;
        let mut spki = ED25519_SPKI.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let key = rustls::crypto::ring::sign::any_supported_type(
            &PrivatePkcs8KeyDer::from(bytes.to_vec()).into(),
        )
        .map_err(io::Error::other)?;
        Ok(Self {
            key: Arc::new(rustls::sign::CertifiedKey::new(
                vec![CertificateDer::from(spki.clone())],
                key,
            )),
            spki,
        })
    }

    pub fn spki(&self) -> &[u8] {
        &self.spki
    }
    pub fn id(&self) -> String {
        fingerprint(&self.spki)
    }
}

pub fn fingerprint(spki: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, spki)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_spki(spki: &[u8]) -> bool {
    spki.len() == ED25519_SPKI.len() + 32 && spki.starts_with(ED25519_SPKI)
}

/// Live pins are checked by the verifier and again at admission. Revocation
/// must also shut down admitted Duplex handles under the admission lock.
#[derive(Debug, Default)]
pub struct Pins(RwLock<HashSet<Vec<u8>>>);

impl Pins {
    pub fn insert(&self, spki: Vec<u8>) -> io::Result<()> {
        if !valid_spki(&spki) {
            return Err(io::Error::other("pin must be an Ed25519 SPKI"));
        }
        self.0.write().unwrap().insert(spki);
        Ok(())
    }
    pub fn remove(&self, spki: &[u8]) {
        self.0.write().unwrap().remove(spki);
    }
    pub fn contains(&self, spki: &[u8]) -> bool {
        self.0.read().unwrap().contains(spki)
    }
}

struct Verifier {
    pins: Arc<Pins>,
    pairing: Option<Arc<crate::pairing::Pairing>>,
    pairing_client: bool,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ed25519 pin verifier")
    }
}

impl Verifier {
    fn verify(
        &self,
        key: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), Error> {
        if !intermediates.is_empty()
            || !valid_spki(key.as_ref())
            || !(self.pins.contains(key.as_ref())
                || self.pairing_client
                || self
                    .pairing
                    .as_ref()
                    .is_some_and(|pairing| pairing.is_open()))
        {
            return Err(Error::General(
                "peer identity is not paired (or has changed); pair again locally".into(),
            ));
        }
        Ok(())
    }
}

fn signature(
    message: &[u8],
    key: &CertificateDer<'_>,
    signature: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    if signature.scheme != SignatureScheme::ED25519 || !valid_spki(key.as_ref()) {
        return Err(Error::General(
            "expected an Ed25519 handshake signature".into(),
        ));
    }
    rustls::crypto::verify_tls13_signature_with_raw_key(
        message,
        &SubjectPublicKeyInfoDer::from(key.as_ref()),
        signature,
        &rustls::crypto::ring::default_provider().signature_verification_algorithms,
    )
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        key: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.verify(key, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _key: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.3 required".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        key: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        signature(message, key, sig)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for Verifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        key: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.verify(key, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _key: &CertificateDer<'_>,
        _sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.3 required".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        key: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        signature(message, key, sig)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

pub fn client_config(identity: &Identity, pins: Arc<Pins>) -> io::Result<Arc<ClientConfig>> {
    client(identity, pins, false)
}

pub fn pairing_client_config(identity: &Identity) -> io::Result<Arc<ClientConfig>> {
    client(identity, Arc::new(Pins::default()), true)
}

fn client(
    identity: &Identity,
    pins: Arc<Pins>,
    pairing_client: bool,
) -> io::Result<Arc<ClientConfig>> {
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Verifier {
                pins,
                pairing: None,
                pairing_client,
            }))
            .with_client_cert_resolver(Arc::new(
                rustls::client::AlwaysResolvesClientRawPublicKeys::new(identity.key.clone()),
            ));
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_sni = false;
    Ok(Arc::new(config))
}

pub fn server_config(
    identity: &Identity,
    pins: Arc<Pins>,
    pairing: Arc<crate::pairing::Pairing>,
) -> io::Result<Arc<ServerConfig>> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_client_cert_verifier(Arc::new(Verifier {
                pins,
                pairing: Some(pairing),
                pairing_client: false,
            }))
            .with_cert_resolver(Arc::new(
                rustls::server::AlwaysResolvesServerRawPublicKeys::new(identity.key.clone()),
            ));
    config.alpn_protocols = vec![ALPN.to_vec()];
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    Ok(Arc::new(config))
}

pub(crate) fn socket_options(socket: &TcpStream) -> io::Result<()> {
    socket.set_nodelay(true)?;
    socket2::SockRef::from(socket).set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(10))
            .with_interval(Duration::from_secs(1))
            .with_retries(3),
    )
}

/// Complete a handshake with an absolute deadline, not a fresh timeout for
/// each read (a slow sender must not extend the two-second budget).
pub(crate) fn handshake(
    mut socket: TcpStream,
    mut connection: rustls::Connection,
    deadline: Instant,
) -> io::Result<(TcpStream, rustls::Connection)> {
    // Darwin can inherit the nonblocking listener's file status flags on
    // accept. Handshakes use blocking I/O with an absolute deadline.
    socket.set_nonblocking(false)?;
    socket_options(&socket)?;
    while connection.is_handshaking() || connection.wants_write() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake deadline"))?;
        socket.set_read_timeout(Some(remaining))?;
        socket.set_write_timeout(Some(remaining))?;
        // One operation at a time: complete_io can perform many reads before
        // returning and would reset the OS timeout for every one of them.
        if connection.wants_write() {
            connection.write_tls(&mut socket)?;
            continue;
        }
        if connection.read_tls(&mut socket)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TLS handshake closed",
            ));
        }
        connection.process_new_packets().map_err(io::Error::other)?;
    }
    if connection.alpn_protocol() != Some(ALPN) {
        return Err(io::Error::other("peer does not speak isocast/1"));
    }
    socket.set_read_timeout(None)?;
    socket.set_write_timeout(None)?;
    Ok((socket, connection))
}

pub fn connect(addr: SocketAddr, config: Arc<ClientConfig>) -> io::Result<Duplex> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    let connection = rustls::ClientConnection::new(config, ServerName::IpAddress(addr.ip().into()))
        .map_err(io::Error::other)?;
    let (socket, connection) = handshake(socket, connection.into(), deadline)?;
    let spki = connection
        .peer_certificates()
        .and_then(|keys| keys.first())
        .ok_or_else(|| io::Error::other("missing peer key"))?
        .as_ref()
        .to_vec();
    Duplex::start(
        socket,
        connection,
        Peer {
            client_id: fingerprint(&spki),
            name: String::new(),
            addr,
            spki,
        },
    )
}
