//! A publishing RTMP client, written from Adobe's public "Real-Time
//! Messaging Protocol (RTMP) specification" (1.0): the handshake, the chunk
//! stream both ways, protocol control messages, and the NetConnection and
//! NetStream commands a live encoder sends (`connect`, `releaseStream`,
//! `FCPublish`, `createStream`, `publish`). RTMPS is the same over TLS.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_protocol::stream::ServerUrl;

use super::amf::{self, Amf};

/// The chunk size this client sends with (the spec's default is 128).
const CHUNK_SIZE: usize = 4096;
/// The acknowledgement window this client asks the server to use.
const WINDOW: u32 = 2_500_000;
/// RTMP version 3, the only one.
const VERSION: u8 = 3;
const HANDSHAKE_SIZE: usize = 1536;
/// How long one write may block before the connection counts as lost.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest one read waits while connecting, so a stop is noticed.
const READ_SLICE: Duration = Duration::from_millis(100);

// Chunk streams: 2 is reserved for protocol control; the rest are ours.
const CONTROL_CSID: u32 = 2;
const COMMAND_CSID: u32 = 3;
const STREAM_CSID: u32 = 4;
const AUDIO_CSID: u32 = 5;
const VIDEO_CSID: u32 = 6;

// Message types.
const SET_CHUNK_SIZE: u8 = 1;
const ABORT: u8 = 2;
const ACKNOWLEDGEMENT: u8 = 3;
const USER_CONTROL: u8 = 4;
const WINDOW_ACK_SIZE: u8 = 5;
const SET_PEER_BANDWIDTH: u8 = 6;
const AUDIO: u8 = 8;
const VIDEO: u8 = 9;
const DATA_AMF0: u8 = 18;
const COMMAND_AMF0: u8 = 20;

// User control events.
const PING_REQUEST: u16 = 6;
const PING_RESPONSE: u16 = 7;

/// A trusted extra certificate authority for RTMPS, for testing against a
/// local server with its own certificate. It only adds trust; the system's
/// roots and every other check stay as they are.
pub const CA_FILE_ENV: &str = "ISOCAST_STREAM_CA_FILE";

/// TCP, or TLS over TCP for RTMPS.
enum Transport {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Transport {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(tcp) => tcp,
            Self::Tls(tls) => tls.get_ref(),
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.read(buf),
            Self::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.write(buf),
            Self::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.flush(),
            Self::Tls(tls) => tls.flush(),
        }
    }
}

/// A connection publishing one live stream.
pub(crate) struct Connection {
    transport: Transport,
    reader: ChunkReader,
    /// The message stream `createStream` gave us.
    stream_id: u32,
    next_transaction: f64,
    /// Bytes received, and the count last acknowledged.
    received: u64,
    acknowledged: u64,
    /// How often the server wants acknowledgements, in bytes.
    server_window: Option<u32>,
    /// Bytes written to the socket, protocol included.
    pub(crate) bytes_sent: u64,
    /// The server's first and latest acknowledgement (when, and its byte
    /// count), for the upload test.
    acks: Option<((Instant, u32), (Instant, u32))>,
}

/// When waiting for the server while connecting ends: at the deadline, or
/// as soon as the stream is stopped.
#[derive(Clone, Copy)]
struct Until<'a> {
    deadline: Instant,
    stop: &'a AtomicBool,
}

impl Until<'_> {
    /// How long the next read may wait, or why waiting is over.
    fn next_wait(&self) -> Result<Duration> {
        if self.stop.load(Ordering::SeqCst) {
            bail!("the stream was stopped while connecting");
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("the server didn't answer in time");
        }
        Ok(remaining.min(READ_SLICE))
    }
}

/// A message read off the chunk stream.
#[derive(Debug)]
struct Message {
    type_id: u8,
    payload: Vec<u8>,
}

impl Connection {
    /// Connect to `server` and start publishing the stream named `key`,
    /// within `timeout`, giving up early once `stop` is set. Errors never
    /// contain the key.
    pub(crate) fn publish(
        server: &ServerUrl,
        key: &str,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<Self> {
        let until = Until {
            deadline: Instant::now() + timeout,
            stop,
        };
        let began = Instant::now();
        let transport = open(server, until)?;
        isocast_diagnostics::debug!("rtmp: connected in {:?}", began.elapsed());
        let mut connection = Self {
            transport,
            reader: ChunkReader::default(),
            stream_id: 0,
            next_transaction: 1.0,
            received: 0,
            acknowledged: 0,
            server_window: None,
            bytes_sent: 0,
            acks: None,
        };
        connection.handshake(until).context("RTMP handshake")?;
        connection.send_control(SET_CHUNK_SIZE, &(CHUNK_SIZE as u32).to_be_bytes())?;
        connection.start(server, key, until)?;
        isocast_diagnostics::debug!("rtmp: publishing after {:?}", began.elapsed());
        connection
            .transport
            .tcp()
            .set_read_timeout(Some(Duration::from_millis(1)))?;
        Ok(connection)
    }

    /// C0+C1 out, S0+S1 in, C2 (an echo of S1) out, S2 in. A zero version
    /// field in C1 asks for the plain handshake.
    fn handshake(&mut self, until: Until) -> Result<()> {
        let mut hello = vec![VERSION];
        hello.extend_from_slice(&[0; 8]);
        hello.extend(random_bytes(HANDSHAKE_SIZE - 8));
        self.write_all(&hello)?;
        let mut reply = vec![0; 1 + HANDSHAKE_SIZE];
        self.read_exact(&mut reply, until)?;
        if reply[0] != VERSION {
            bail!(
                "the server answered with RTMP version {} instead of {VERSION}; is this an RTMP \
                 server?",
                reply[0]
            );
        }
        let echo = reply[1..].to_vec();
        self.write_all(&echo)?;
        let mut s2 = vec![0; HANDSHAKE_SIZE];
        self.read_exact(&mut s2, until)?;
        Ok(())
    }

    /// `connect`, then `releaseStream`, `FCPublish`, `createStream` and
    /// `publish`, waiting for the server's answers.
    fn start(&mut self, server: &ServerUrl, key: &str, until: Until) -> Result<()> {
        let url = server.to_string();
        let connect = self.command(
            "connect",
            &[Amf::object(&[
                ("app", Amf::string(&server.app)),
                ("type", Amf::string("nonprivate")),
                ("flashVer", Amf::string("FMLE/3.0 (compatible; FMSc/1.0)")),
                ("swfUrl", Amf::string(&url)),
                ("tcUrl", Amf::string(&url)),
            ])],
            0,
        )?;
        let reply = self.wait_for_result(connect, until).context("connect")?;
        if let Some(code) = info(&reply)
            .and_then(|i| i.get("code"))
            .and_then(Amf::as_str)
            && code != "NetConnection.Connect.Success"
        {
            bail!("the server refused the connection: {}", describe(&reply));
        }
        self.command("releaseStream", &[Amf::Null, Amf::string(key)], 0)?;
        self.command("FCPublish", &[Amf::Null, Amf::string(key)], 0)?;
        let create = self.command("createStream", &[Amf::Null], 0)?;
        let reply = self
            .wait_for_result(create, until)
            .context("createStream")?;
        self.stream_id = reply
            .get(3)
            .and_then(Amf::as_f64)
            .context("createStream answered without a stream id")? as u32;
        let stream = self.stream_id;
        self.command(
            "publish",
            &[Amf::Null, Amf::string(key), Amf::string("live")],
            stream,
        )?;
        loop {
            let values = self.next_command(until).context("publish")?;
            let name = values.first().and_then(Amf::as_str).unwrap_or_default();
            let info = info(&values);
            let code = info
                .and_then(|i| i.get("code"))
                .and_then(Amf::as_str)
                .unwrap_or_default();
            let level = info
                .and_then(|i| i.get("level"))
                .and_then(Amf::as_str)
                .unwrap_or_default();
            match name {
                "onStatus" if code == "NetStream.Publish.Start" => return Ok(()),
                "onStatus" if level == "error" || code.contains("Fail") || code.contains("Bad") => {
                    bail!(
                        "the server refused to publish: {}; check the stream key",
                        describe(&values)
                    )
                }
                "_error" | "close" => bail!("the server refused to publish: {}", describe(&values)),
                _ => {}
            }
        }
    }

    /// Send a command (`_result` comes back with its transaction number).
    fn command(&mut self, name: &str, arguments: &[Amf], stream: u32) -> Result<f64> {
        let transaction = self.next_transaction;
        self.next_transaction += 1.0;
        let mut values = vec![Amf::string(name), Amf::Number(transaction)];
        values.extend_from_slice(arguments);
        let csid = if stream == 0 {
            COMMAND_CSID
        } else {
            STREAM_CSID
        };
        self.send(csid, COMMAND_AMF0, stream, 0, &amf::encode(&values))?;
        Ok(transaction)
    }

    /// The `_result` of `transaction`, or an error for its `_error`.
    fn wait_for_result(&mut self, transaction: f64, until: Until) -> Result<Vec<Amf>> {
        loop {
            let values = self.next_command(until)?;
            let name = values.first().and_then(Amf::as_str).unwrap_or_default();
            let id = values.get(1).and_then(Amf::as_f64);
            match name {
                "_result" if id == Some(transaction) => return Ok(values),
                "_error" if id == Some(transaction) => {
                    bail!("the server refused: {}", describe(&values))
                }
                "close" => bail!("the server closed the connection: {}", describe(&values)),
                _ => {}
            }
        }
    }

    /// The next command message, handling control messages on the way.
    fn next_command(&mut self, until: Until) -> Result<Vec<Amf>> {
        loop {
            let message = self.next_message(until)?;
            if message.type_id == COMMAND_AMF0 {
                return amf::decode(&message.payload);
            }
        }
    }

    /// The next message other than protocol control, which is answered
    /// here.
    fn next_message(&mut self, until: Until) -> Result<Message> {
        loop {
            if let Some(message) = self.reader.next()? {
                if let Some(message) = self.handle(message)? {
                    return Ok(message);
                }
                continue;
            }
            let wait = until.next_wait()?;
            self.transport.tcp().set_read_timeout(Some(wait))?;
            self.fill()?;
        }
    }

    /// Read what has arrived into the chunk reader. Returns whether
    /// anything came (`false` when the read timed out).
    fn fill(&mut self) -> Result<bool> {
        let mut buffer = [0u8; 16 * 1024];
        match self.transport.read(&mut buffer) {
            Ok(0) => bail!("the server closed the connection"),
            Ok(n) => {
                self.received += n as u64;
                self.reader.push(&buffer[..n]);
                self.acknowledge()?;
                Ok(true)
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(false)
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(true),
            Err(err) => Err(err).context("lost the connection"),
        }
    }

    /// Acknowledge received bytes once a window's worth has come.
    fn acknowledge(&mut self) -> Result<()> {
        if let Some(window) = self.server_window
            && self.received - self.acknowledged >= u64::from(window)
        {
            self.acknowledged = self.received;
            let count = (self.received & 0xffff_ffff) as u32;
            self.send_control(ACKNOWLEDGEMENT, &count.to_be_bytes())?;
        }
        Ok(())
    }

    /// Act on protocol control messages; anything else is returned.
    fn handle(&mut self, message: Message) -> Result<Option<Message>> {
        let payload = &message.payload;
        let u32_at = |at: usize| {
            payload
                .get(at..at + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        };
        match message.type_id {
            SET_CHUNK_SIZE => {
                let size = u32_at(0).context("a short Set Chunk Size message")? & 0x7fff_ffff;
                self.reader.chunk_size = (size as usize).max(1);
            }
            ABORT => {
                if let Some(csid) = u32_at(0) {
                    self.reader.abort(csid);
                }
            }
            WINDOW_ACK_SIZE => self.server_window = u32_at(0).filter(|w| *w > 0),
            SET_PEER_BANDWIDTH => {
                self.send_control(WINDOW_ACK_SIZE, &WINDOW.to_be_bytes())?;
            }
            USER_CONTROL => {
                let event = payload.get(0..2).map(|b| u16::from_be_bytes([b[0], b[1]]));
                if event == Some(PING_REQUEST) {
                    let mut pong = PING_RESPONSE.to_be_bytes().to_vec();
                    pong.extend_from_slice(payload.get(2..6).unwrap_or(&[0; 4]));
                    self.send_control(USER_CONTROL, &pong)?;
                }
            }
            ACKNOWLEDGEMENT => {
                if let Some(count) = u32_at(0) {
                    let now = (Instant::now(), count);
                    self.acks = Some(self.acks.map_or((now, now), |(first, _)| (first, now)));
                }
            }
            _ => return Ok(Some(message)),
        }
        Ok(None)
    }

    /// Answer whatever the server sent since the last call, without
    /// waiting. Fails if the server closed the connection or refused the
    /// stream.
    pub(crate) fn poll(&mut self) -> Result<()> {
        while self.fill()? {}
        while let Some(message) = self.reader.next()? {
            let Some(message) = self.handle(message)? else {
                continue;
            };
            if message.type_id != COMMAND_AMF0 {
                continue;
            }
            let values = amf::decode(&message.payload)?;
            let name = values.first().and_then(Amf::as_str).unwrap_or_default();
            let level = info(&values)
                .and_then(|i| i.get("level"))
                .and_then(Amf::as_str)
                .unwrap_or_default();
            if name == "close" || (name == "onStatus" && level == "error") {
                bail!("the server stopped the stream: {}", describe(&values));
            }
        }
        Ok(())
    }

    /// Ask the server to acknowledge every `window` bytes it receives (the
    /// upload test measures by them).
    pub(crate) fn ask_acknowledgements(&mut self, window: u32) -> Result<()> {
        self.send_control(WINDOW_ACK_SIZE, &window.to_be_bytes())
    }

    /// The server's first and latest acknowledgement: when, and how many
    /// bytes it had received.
    pub(crate) fn acknowledgements(&self) -> Option<((Instant, u32), (Instant, u32))> {
        self.acks
    }

    /// Describe the stream (`@setDataFrame onMetaData`).
    pub(crate) fn send_metadata(&mut self, body: &[u8]) -> Result<()> {
        self.send(STREAM_CSID, DATA_AMF0, self.stream_id, 0, body)
    }

    pub(crate) fn send_audio(&mut self, timestamp_ms: u32, body: &[u8]) -> Result<()> {
        self.send(AUDIO_CSID, AUDIO, self.stream_id, timestamp_ms, body)
    }

    pub(crate) fn send_video(&mut self, timestamp_ms: u32, body: &[u8]) -> Result<()> {
        self.send(VIDEO_CSID, VIDEO, self.stream_id, timestamp_ms, body)
    }

    /// Another handle on the connection's socket.
    pub(crate) fn socket(&self) -> io::Result<TcpStream> {
        self.transport.tcp().try_clone()
    }

    /// Say goodbye (`FCUnpublish`, `deleteStream`) and close.
    pub(crate) fn close(mut self, key: &str) {
        let stream = f64::from(self.stream_id);
        let _ = self.command("FCUnpublish", &[Amf::Null, Amf::string(key)], 0);
        let _ = self.command("deleteStream", &[Amf::Null, Amf::Number(stream)], 0);
        let _ = self.transport.flush();
        let _ = self.transport.tcp().shutdown(std::net::Shutdown::Both);
    }

    fn send_control(&mut self, type_id: u8, payload: &[u8]) -> Result<()> {
        self.send(CONTROL_CSID, type_id, 0, 0, payload)
    }

    /// Write one message, cut into chunks, in a single write.
    fn send(
        &mut self,
        csid: u32,
        type_id: u8,
        stream: u32,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<()> {
        let bytes = chunks(csid, type_id, stream, timestamp, payload);
        self.write_all(&bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.transport
            .write_all(bytes)
            .context("lost the connection (sending)")?;
        self.bytes_sent += bytes.len() as u64;
        Ok(())
    }

    fn read_exact(&mut self, buffer: &mut [u8], until: Until) -> Result<()> {
        let mut at = 0;
        while at < buffer.len() {
            let wait = until.next_wait()?;
            self.transport.tcp().set_read_timeout(Some(wait))?;
            match self.transport.read(&mut buffer[at..]) {
                Ok(0) => bail!("the server closed the connection"),
                Ok(n) => {
                    at += n;
                    self.received += n as u64;
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(err) => return Err(err).context("lost the connection"),
            }
        }
        Ok(())
    }
}

/// Open TCP (and TLS for RTMPS) to the server before `until`.
fn open(server: &ServerUrl, until: Until) -> Result<Transport> {
    let host = server.host.as_str();
    let tcp = connect(server, until)?;
    tcp.set_nodelay(true)?;
    tcp.set_write_timeout(Some(WRITE_TIMEOUT))?;
    if !server.secure {
        return Ok(Transport::Plain(tcp));
    }
    let mut builder = native_tls::TlsConnector::builder();
    if let Some(path) = std::env::var_os(CA_FILE_ENV) {
        let pem = std::fs::read(&path)
            .with_context(|| format!("could not read {CA_FILE_ENV} ({})", path.display()))?;
        let ca = native_tls::Certificate::from_pem(&pem).with_context(|| {
            format!(
                "{CA_FILE_ENV} ({}) is not a PEM certificate",
                path.display()
            )
        })?;
        builder.add_root_certificate(ca);
    }
    let connector = builder.build().context("could not set up TLS")?;
    tcp.set_read_timeout(Some(until.next_wait()?))?;
    let mut attempt = connector.connect(host, tcp);
    loop {
        match attempt {
            Ok(tls) => return Ok(Transport::Tls(Box::new(tls))),
            Err(native_tls::HandshakeError::Failure(err)) => {
                bail!("TLS with {host} failed: {err}")
            }
            Err(native_tls::HandshakeError::WouldBlock(mid)) => {
                until
                    .next_wait()
                    .with_context(|| format!("TLS with {host} didn't finish"))?;
                attempt = mid.handshake();
            }
        }
    }
}

/// Look up the server and open TCP to it before `until`. Both run on a
/// thread of their own, so a stop doesn't wait for an unanswered connect
/// (which is left to time out alone).
fn connect(server: &ServerUrl, until: Until) -> Result<TcpStream> {
    let (host, port, deadline) = (server.host.clone(), server.port, until.deadline);
    let (tx, rx) = mpsc::channel();
    let failed = tx.clone();
    isocast_diagnostics::spawn(
        "stream connect",
        move || {
            let _ = tx.send(connect_now(&host, port, deadline));
        },
        move |why| {
            let _ = failed.send(Err(anyhow!(why)));
        },
    )?;
    loop {
        if until.stop.load(Ordering::SeqCst) {
            bail!("the stream was stopped while connecting");
        }
        // The connect itself can outlast its deadline (macOS holds it while
        // its Local Network prompt waits for an answer, #65; a lookup has
        // no deadline of its own): give up on it here, as for a stop.
        if Instant::now() >= deadline + CONNECT_GRACE {
            let hint = if maybe_local(&server.host) {
                LOCAL_NETWORK_HINT
            } else {
                ""
            };
            bail!(
                "could not connect to {} port {port} in time{hint}",
                server.host
            );
        }
        match rx.recv_timeout(READ_SLICE) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                bail!("could not connect to {} port {port}", server.host)
            }
        }
    }
}

/// Look up `host` (IPv4 and IPv6) and try each address in turn, the
/// families alternating, until one connects (#57).
fn connect_now(host: &str, port: u16, deadline: Instant) -> Result<TcpStream> {
    let found: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|err| {
            let hint = if host.ends_with(".local") || host.ends_with(".local.") {
                LOCAL_NETWORK_HINT
            } else {
                ""
            };
            anyhow!("could not look up {host}: {err}{hint}")
        })?
        .collect();
    let addresses = alternate_families(found);
    let mut failures: Vec<(SocketAddr, io::Error)> = Vec::new();
    for (i, address) in addresses.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // An address that never answers mustn't use up the time the
        // others have; the last one gets all that's left.
        let wait = if i + 1 < addresses.len() {
            remaining.min(ATTEMPT_TIMEOUT)
        } else {
            remaining
        };
        match TcpStream::connect_timeout(address, wait) {
            Ok(stream) => return Ok(stream),
            Err(err) => failures.push((*address, err)),
        }
    }
    let tried = addresses
        .iter()
        .map(|a| a.ip().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    // macOS's Local Network privacy refuses a local address with
    // EHOSTUNREACH, without a prompt if the app can't ask: whichever
    // address it was, not just the last one tried (#65).
    let hint = if failures.iter().any(|(a, err)| blocked_locally(a, err)) {
        LOCAL_NETWORK_HINT
    } else {
        ""
    };
    Err(match failures.as_slice() {
        [] => anyhow!("could not connect to {host} port {port} in time{hint}"),
        [.., (_, last)]
            if failures
                .iter()
                .all(|(_, err)| err.to_string() == last.to_string()) =>
        {
            anyhow!("could not connect to {host} port {port} (tried {tried}): {last}{hint}")
        }
        // Different failures: each address's.
        _ => {
            let each = failures
                .iter()
                .map(|(a, err)| format!("{}: {err}", a.ip()))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow!("could not connect to {host} port {port} ({each}){hint}")
        }
    })
}

/// How long past its deadline a connect's answer is still waited for.
const CONNECT_GRACE: Duration = Duration::from_secs(1);

/// The longest one address may take to connect while others remain.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

/// What to do when macOS keeps Isocast off the local network.
const LOCAL_NETWORK_HINT: &str = if cfg!(target_os = "macos") {
    "; if the server is on your local network, macOS may be blocking Isocast from it: turn on \
     Isocast (or Isocast Engine) in System Settings → Privacy & Security → Local Network, then \
     start the stream again"
} else {
    ""
};

/// `addresses` in the resolver's order, but IPv6 and IPv4 taking turns, so
/// a family that doesn't work costs one attempt rather than all of them.
fn alternate_families(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut unique = Vec::new();
    for address in addresses {
        if !unique.contains(&address) {
            unique.push(address);
        }
    }
    let first_v6 = unique.first().is_some_and(SocketAddr::is_ipv6);
    let (mut first, mut second): (Vec<_>, Vec<_>) =
        unique.into_iter().partition(|a| a.is_ipv6() == first_v6);
    first.reverse();
    second.reverse();
    let mut out = Vec::with_capacity(first.len() + second.len());
    while let Some(a) = first.pop() {
        out.push(a);
        if let Some(b) = second.pop() {
            out.push(b);
        }
    }
    out.extend(second.into_iter().rev());
    out
}

/// Whether `err` connecting to `address` looks like the local network
/// being off limits: "no route" to a private or link-local address.
fn blocked_locally(address: &SocketAddr, err: &io::Error) -> bool {
    let unreachable = matches!(
        err.kind(),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable
    );
    unreachable && is_local(address.ip())
}

/// A private, link-local or unique-local address, or (IPv6) one on the
/// same link as this Mac by its interfaces' prefixes: global IPv6 LAN
/// addresses are local networks too (#65).
fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_unicast_link_local() || ip.is_unique_local() || on_link(ip),
    }
}

/// `host` (as given) may be on the local network: a `.local` name or a
/// local address.
fn maybe_local(host: &str) -> bool {
    host.ends_with(".local")
        || host.ends_with(".local.")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .is_ok_and(is_local)
}

/// Whether `ip` is inside the prefix of one of this Mac's IPv6 interface
/// addresses (its own subnet).
#[cfg(unix)]
fn on_link(ip: std::net::Ipv6Addr) -> bool {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below; each entry's addr and
    // netmask, when set, point at sockaddrs of the entry's family.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return false;
        }
        let mut found = false;
        let mut entry = list;
        while !entry.is_null() && !found {
            let e = &*entry;
            if !e.ifa_addr.is_null()
                && !e.ifa_netmask.is_null()
                && i32::from((*e.ifa_addr).sa_family) == libc::AF_INET6
            {
                let addr = (*e.ifa_addr.cast::<libc::sockaddr_in6>()).sin6_addr.s6_addr;
                let mask = (*e.ifa_netmask.cast::<libc::sockaddr_in6>())
                    .sin6_addr
                    .s6_addr;
                found = same_prefix(&ip.octets(), &addr, &mask);
            }
            entry = e.ifa_next;
        }
        libc::freeifaddrs(list);
        found
    }
}

#[cfg(not(unix))]
fn on_link(_ip: std::net::Ipv6Addr) -> bool {
    false
}

/// `a` and `b` agree on the bits `mask` sets (a mask of all zeros, as a
/// default route's, matches nothing).
fn same_prefix(a: &[u8; 16], b: &[u8; 16], mask: &[u8; 16]) -> bool {
    mask.iter().any(|&m| m != 0) && (0..16).all(|i| a[i] & mask[i] == b[i] & mask[i])
}

/// The info object of a `_result`, `_error` or `onStatus`: the first
/// object after the transaction id.
fn info(values: &[Amf]) -> Option<&Amf> {
    values
        .iter()
        .skip(2)
        .find(|value| matches!(value, Amf::Object(_)))
}

/// A server's code and description, for errors.
fn describe(values: &[Amf]) -> String {
    let info = info(values);
    let field = |name| info.and_then(|i| i.get(name)).and_then(Amf::as_str);
    match (field("code"), field("description")) {
        (Some(code), Some(text)) if !text.is_empty() => format!("{code} ({text})"),
        (Some(code), _) => code.to_string(),
        (None, Some(text)) => text.to_string(),
        (None, None) => values
            .first()
            .and_then(Amf::as_str)
            .unwrap_or("no reason given")
            .to_string(),
    }
}

/// One message as chunks: a type 0 header first, type 3 headers after.
fn chunks(csid: u32, type_id: u8, stream: u32, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!((2..64).contains(&csid));
    let extended = timestamp >= 0xff_ffff;
    let mut out = Vec::with_capacity(payload.len() + 16 + payload.len() / CHUNK_SIZE * 5);
    out.push(csid as u8);
    let stamp = if extended { 0xff_ffff } else { timestamp };
    out.extend_from_slice(&stamp.to_be_bytes()[1..]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.push(type_id);
    out.extend_from_slice(&stream.to_le_bytes());
    if extended {
        out.extend_from_slice(&timestamp.to_be_bytes());
    }
    for (i, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
        if i > 0 {
            out.push(0xc0 | csid as u8);
            if extended {
                out.extend_from_slice(&timestamp.to_be_bytes());
            }
        }
        out.extend_from_slice(chunk);
    }
    out
}

/// Reassembles messages from the server's chunk stream.
struct ChunkReader {
    buffer: Vec<u8>,
    chunk_size: usize,
    streams: HashMap<u32, InStream>,
}

impl Default for ChunkReader {
    fn default() -> Self {
        Self {
            buffer: Vec::new(),
            chunk_size: 128,
            streams: HashMap::new(),
        }
    }
}

/// The last header seen on one chunk stream, and a message in progress.
#[derive(Default)]
struct InStream {
    timestamp: u32,
    delta: u32,
    length: usize,
    type_id: u8,
    stream: u32,
    extended: bool,
    payload: Vec<u8>,
}

impl ChunkReader {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    fn abort(&mut self, csid: u32) {
        if let Some(stream) = self.streams.get_mut(&csid) {
            stream.payload.clear();
        }
    }

    /// The next complete message, if the buffer holds one.
    fn next(&mut self) -> Result<Option<Message>> {
        loop {
            match self.chunk()? {
                Some(Some(message)) => return Ok(Some(message)),
                Some(None) => continue,
                None => return Ok(None),
            }
        }
    }

    /// Parse one chunk: `None` if it hasn't fully arrived, `Some(None)` if
    /// it was part of a message still in progress.
    fn chunk(&mut self) -> Result<Option<Option<Message>>> {
        let b = &self.buffer;
        let Some(&first) = b.first() else {
            return Ok(None);
        };
        let format = first >> 6;
        let (csid, mut at) = match first & 0x3f {
            0 => match b.get(1) {
                Some(&next) => (64 + u32::from(next), 2),
                None => return Ok(None),
            },
            1 => match b.get(1..3) {
                Some(next) => (64 + u32::from(next[0]) + 256 * u32::from(next[1]), 3),
                None => return Ok(None),
            },
            id => (u32::from(id), 1),
        };
        let header = [11, 7, 3, 0][usize::from(format)];
        let Some(fields) = b.get(at..at + header) else {
            return Ok(None);
        };
        let u24 = |f: &[u8]| u32::from_be_bytes([0, f[0], f[1], f[2]]);
        let previous = self.streams.get(&csid);
        if format != 0 && previous.is_none() {
            bail!("the server continued chunk stream {csid} before starting it");
        }
        let mut state = previous.map_or_else(InStream::default, |p| InStream {
            payload: Vec::new(),
            ..*p
        });
        let stamp = if header >= 3 { u24(fields) } else { 0 };
        if header >= 7 {
            state.length = u24(&fields[3..]) as usize;
            state.type_id = fields[6];
        }
        if header == 11 {
            state.stream = u32::from_le_bytes([fields[7], fields[8], fields[9], fields[10]]);
        }
        at += header;
        let extended = match format {
            3 => state.extended,
            _ => stamp == 0xff_ffff,
        };
        let mut stamp = stamp;
        if extended {
            let Some(ext) = b.get(at..at + 4) else {
                return Ok(None);
            };
            stamp = u32::from_be_bytes([ext[0], ext[1], ext[2], ext[3]]);
            at += 4;
        }
        let progress = previous.map_or(0, |p| p.payload.len());
        let continuing = format == 3 && progress > 0;
        let done = if continuing { progress } else { 0 };
        let size = (state.length - done).min(self.chunk_size);
        if b.len() < at + size {
            return Ok(None);
        }
        let data = b[at..at + size].to_vec();
        self.buffer.drain(..at + size);
        let entry = self.streams.entry(csid).or_default();
        if continuing {
            entry.payload.extend_from_slice(&data);
        } else {
            match format {
                0 => {
                    state.timestamp = stamp;
                    state.delta = 0;
                }
                1 | 2 => {
                    state.delta = stamp;
                    state.timestamp = state.timestamp.wrapping_add(stamp);
                }
                _ => state.timestamp = state.timestamp.wrapping_add(state.delta),
            }
            state.extended = extended;
            state.payload = data;
            *entry = state;
        }
        if entry.payload.len() >= entry.length {
            let payload = std::mem::take(&mut entry.payload);
            return Ok(Some(Some(Message {
                type_id: entry.type_id,
                payload,
            })));
        }
        Ok(Some(None))
    }
}

/// Bytes for the handshake's random field. They only need to differ
/// between connections, not be secret.
fn random_bytes(n: usize) -> Vec<u8> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x = seed ^ 0x9e37_79b9_7f4a_7c15 ^ (u64::from(std::process::id()) << 32);
    (0..n)
        .map(|_| {
            // xorshift64
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_alternate_families_and_name_local_network_trouble() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let found = vec![
            a("[fe80::1%1]:1935"),
            a("[fd00::2]:1935"),
            a("[fd00::3]:1935"),
            a("10.0.0.5:1935"),
            a("[fe80::1%1]:1935"),
        ];
        assert_eq!(
            alternate_families(found),
            vec![
                a("[fe80::1%1]:1935"),
                a("10.0.0.5:1935"),
                a("[fd00::2]:1935"),
                a("[fd00::3]:1935"),
            ]
        );
        assert_eq!(
            alternate_families(vec![a("10.0.0.5:1"), a("10.0.0.6:1"), a("[::1]:1")]),
            vec![a("10.0.0.5:1"), a("[::1]:1"), a("10.0.0.6:1")]
        );
        let unreachable = io::Error::from_raw_os_error(libc::EHOSTUNREACH);
        assert!(blocked_locally(&a("10.0.10.165:1935"), &unreachable));
        assert!(blocked_locally(&a("[fe80::1%1]:1935"), &unreachable));
        assert!(!blocked_locally(&a("8.8.8.8:1935"), &unreachable));
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert!(!blocked_locally(&a("10.0.10.165:1935"), &refused));
        let v6 = |s: &str| s.parse::<std::net::Ipv6Addr>().unwrap().octets();
        let slash64 = v6("ffff:ffff:ffff:ffff::");
        assert!(same_prefix(
            &v6("2001:db8:1:2::5"),
            &v6("2001:db8:1:2::9"),
            &slash64
        ));
        assert!(!same_prefix(
            &v6("2001:db8:1:3::5"),
            &v6("2001:db8:1:2::9"),
            &slash64
        ));
        assert!(!same_prefix(
            &v6("2001:db8:1:3::5"),
            &v6("2001:db8:1:2::9"),
            &[0; 16]
        ));
        assert!(maybe_local("studio.local") && maybe_local("[fd00::2]") && maybe_local("10.0.0.5"));
        assert!(!maybe_local("live.twitch.tv") && !maybe_local("8.8.8.8"));
    }

    #[test]
    fn messages_round_trip_through_chunks_of_any_size() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let bytes = chunks(VIDEO_CSID, VIDEO, 1, 1234, &payload);
        // A type 0 header, then two type 3 continuations.
        assert_eq!(bytes.len(), 12 + payload.len() + 2);
        assert_eq!(&bytes[..4], &[VIDEO_CSID as u8, 0, 0x04, 0xd2]);
        let mut reader = ChunkReader {
            chunk_size: CHUNK_SIZE,
            ..ChunkReader::default()
        };
        // Arriving a few bytes at a time.
        let mut got = None;
        for piece in bytes.chunks(7) {
            reader.push(piece);
            if let Some(message) = reader.next().unwrap() {
                got = Some(message);
            }
        }
        let message = got.expect("a message");
        assert_eq!(message.type_id, VIDEO);
        assert_eq!(message.payload, payload);
        assert_eq!(reader.streams[&VIDEO_CSID].timestamp, 1234);
    }

    #[test]
    fn extended_timestamps_and_header_compression_are_read() {
        let mut reader = ChunkReader::default();
        // Type 0 with an extended timestamp, then a type 2 delta and a type
        // 3 repeat, each a one-byte audio message.
        let mut bytes = chunks(AUDIO_CSID, AUDIO, 1, 0x0100_0000, &[1]);
        bytes.extend_from_slice(&[0x80 | AUDIO_CSID as u8, 0, 0, 20, 2]);
        bytes.extend_from_slice(&[0xc0 | AUDIO_CSID as u8, 3]);
        reader.push(&bytes);
        let mut stamps = Vec::new();
        while let Some(message) = reader.next().unwrap() {
            stamps.push((message.payload[0], reader.streams[&AUDIO_CSID].timestamp));
        }
        assert_eq!(
            stamps,
            [(1, 0x0100_0000), (2, 0x0100_0014), (3, 0x0100_0028)]
        );
    }

    #[test]
    fn server_answers_are_described_by_code_and_text() {
        let values = vec![
            Amf::string("onStatus"),
            Amf::Number(0.0),
            Amf::Null,
            Amf::object(&[
                ("level", Amf::string("error")),
                ("code", Amf::string("NetStream.Publish.BadName")),
                ("description", Amf::string("already publishing")),
            ]),
        ];
        assert_eq!(
            describe(&values),
            "NetStream.Publish.BadName (already publishing)"
        );
    }
}
