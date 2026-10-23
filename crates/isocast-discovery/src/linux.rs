//! Avahi's system D-Bus API. Each handle owns a thread and a current-thread
//! runtime, not a UDP socket or a libavahi client. Dropping the handle cancels
//! the entire session (including setup and RPCs), drops its runtime, and joins.
//! Avahi frees client-owned objects when that private bus connection closes.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{Stream, StreamExt, stream::FuturesUnordered};
use tokio::sync::oneshot;
use zbus::{
    Connection, MatchRule, Message, MessageStream, message::Type, zvariant::OwnedObjectPath,
};

use crate::{Event, Found, MAX_ENGINES, MAX_NAME_BYTES, OnEvent, SERVICE_TYPE, Txt};

pub const SUPPORTED: bool = true;
const AVAHI: &str = "org.freedesktop.Avahi";
const SERVER: &str = "org.freedesktop.Avahi.Server";
const GROUP: &str = "org.freedesktop.Avahi.EntryGroup";
const BROWSER: &str = "org.freedesktop.Avahi.ServiceBrowser";
const DBUS: &str = "org.freedesktop.DBus";
const RPC_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTHY_SESSION: Duration = Duration::from_secs(60);
const RECHECK: Duration = Duration::from_secs(30);
const MAX_INSTANCES: usize = 256;
const MAX_RESOLVING: usize = 4;
const SIGNAL_QUEUE: usize = 256;
const MAX_ADDRESSES: usize = 16;

async fn timed<T>(future: impl Future<Output = zbus::Result<T>>) -> Result<T> {
    tokio::time::timeout(RPC_TIMEOUT, future)
        .await
        .context("Avahi D-Bus call timed out after 2 seconds")?
        .map_err(Into::into)
}

// Keep the wire types visible at each call, without adding serde bounds or
// proxy tasks to this small backend. The deadline includes sending the call.
macro_rules! call {
    ($bus:expr, $path:expr, $interface:expr, $method:expr, $body:expr) => {
        timed($bus.conn.call_method(
            Some($bus.owner.as_str()),
            $path,
            Some($interface),
            $method,
            $body,
        ))
        .await
    };
}

fn unavailable(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("discovery isn't available here; connect by address (Avahi: {error})")
}

struct Worker {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            // A callback may drop its own watch. It already runs on this
            // worker; don't join ourselves. Stop wins before another callback.
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

pub struct Registration {
    name: Arc<Mutex<String>>,
    error: Arc<Mutex<Option<String>>>,
    _worker: Worker,
}

impl Registration {
    pub fn name(&self) -> String {
        self.name.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct Watch {
    _worker: Worker,
}

enum Operation {
    Register {
        name: Arc<Mutex<String>>,
        error: Arc<Mutex<Option<String>>>,
        port: u16,
        txt: Txt,
    },
    Watch {
        on_event: OnEvent,
        published: BTreeMap<String, Found>,
    },
}

pub fn register(name: &str, port: u16, txt: &Txt) -> Result<Registration> {
    let name = Arc::new(Mutex::new(name.to_owned()));
    let error = Arc::new(Mutex::new(Some(
        unavailable("waiting for avahi-daemon; retrying in background").to_string(),
    )));
    let worker = start(Operation::Register {
        name: name.clone(),
        error: error.clone(),
        port,
        txt: txt.clone(),
    })?;
    Ok(Registration {
        name,
        error,
        _worker: worker,
    })
}

pub fn watch(on_event: OnEvent) -> Result<Watch> {
    Ok(Watch {
        _worker: start(Operation::Watch {
            on_event,
            published: BTreeMap::new(),
        })?,
    })
}

struct Ready {
    send: Option<mpsc::SyncSender<Result<()>>>,
    healthy_since: Option<Instant>,
}

fn ready(send: &mut Ready) {
    send.healthy_since.get_or_insert_with(Instant::now);
    if let Some(send) = send.send.take() {
        let _ = send.send(Ok(()));
    }
}

fn start(mut operation: Operation) -> Result<Worker> {
    let registration = matches!(operation, Operation::Register { .. });
    let (stop, mut stopped) = oneshot::channel();
    let (send, receive) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("isocast-avahi".into())
        .spawn(move || {
            let mut send = Ready { send: Some(send), healthy_since: None };
            let mut failures = 0u32;
            loop {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        if let Operation::Register { error: state, .. } = &operation {
                            *state.lock().unwrap_or_else(|e| e.into_inner()) = Some(unavailable(&error).to_string());
                        }
                        if let Some(send) = send.send.take() {
                            let _ = send.send(Err(unavailable(error)));
                        }
                        return;
                    }
                };
                let result = runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = &mut stopped => None,
                        result = async {
                            if failures > 0 {
                                tokio::time::sleep(backoff(failures - 1)).await;
                            }
                            send.healthy_since = None;
                            session(&mut operation, &mut send).await
                        } => Some(result),
                    }
                });
                // Also cancels zbus's socket tasks, including when setup was
                // interrupted before it could return a Connection to us.
                runtime.shutdown_timeout(Duration::from_millis(100));
                let Some(result) = result else { return };
                let error = result
                    .err()
                    .unwrap_or_else(|| anyhow!("Avahi connection ended"));
                if let Operation::Register { error: state, .. } = &operation {
                    *state.lock().unwrap_or_else(|e| e.into_inner()) = Some(unavailable(format!("{error:#}; retrying in background")).to_string());
                    // Registration is persistent intent: a boot-time missing
                    // responder must not strand a running remote listener.
                    if let Some(send) = send.send.take() {
                        let _ = send.send(Ok(()));
                    }
                } else if let Some(send) = send.send.take() {
                    let _ = send.send(Err(unavailable(format!("{error:#}"))));
                    return;
                }
                failures = retry_failures(failures, send.healthy_since.map(|since| since.elapsed()));
                if let Operation::Watch {
                    on_event,
                    published,
                } = &mut operation
                {
                    for name in std::mem::take(published).into_keys() {
                        if stopped.try_recv().is_ok() || stopped.is_terminated() {
                            return;
                        }
                        on_event(Event::Lost { name });
                    }
                    if stopped.try_recv().is_ok() || stopped.is_terminated() {
                        return;
                    }
                    on_event(Event::Error(unavailable(format!("{error:#}")).to_string()));
                } else {
                    isocast_diagnostics::diagnostic!("warn"; "discovery: {error:#}; retrying Avahi registration");
                }
            }
        })
        .context("start Avahi discovery worker")?;
    let worker = Worker {
        stop: Some(stop),
        thread: Some(thread),
    };
    match receive.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => Ok(worker),
        Ok(Err(error)) => Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) if registration => Ok(worker),
        Err(error) => Err(unavailable(format!(
            "startup did not complete within 5 seconds: {error}"
        ))),
    }
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.min(5)).min(30))
}

fn retry_failures(failures: u32, uptime: Option<Duration>) -> u32 {
    if uptime.is_some_and(|uptime| uptime >= HEALTHY_SESSION) {
        1
    } else {
        failures.saturating_add(1)
    }
}

struct Bus {
    conn: Connection,
    owner: String,
    owners: MessageStream,
    signals: MessageStream,
}

impl Bus {
    async fn open(interface: &str) -> Result<Self> {
        let conn = timed(Connection::system())
            .await
            .context("connect to the system bus")?;
        // Subscribe before asking for the owner, then pin *both* calls and
        // signals to that unique owner. A replacement daemon is a new session.
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(DBUS)?
            .interface(DBUS)?
            .path("/org/freedesktop/DBus")?
            .member("NameOwnerChanged")?
            .arg(0, AVAHI)?
            .build();
        let owners = timed(MessageStream::for_match_rule(rule, &conn, Some(16))).await?;
        let owner: String = timed(conn.call_method(
            Some(DBUS),
            "/org/freedesktop/DBus",
            Some(DBUS),
            "GetNameOwner",
            &(AVAHI,),
        ))
        .await
        .context("avahi-daemon is not running")?
        .body()
        .deserialize()?;
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(owner.as_str())?
            .interface(interface)?
            .build();
        // No object path yet: cached ItemNew / StateChanged can precede the
        // New method's reply. Filter by the returned path when consuming.
        let signals = timed(MessageStream::for_match_rule(
            rule,
            &conn,
            Some(SIGNAL_QUEUE),
        ))
        .await?;
        let bus = Self {
            conn,
            owner,
            owners,
            signals,
        };
        tokio::time::timeout(START_TIMEOUT, async {
            loop {
                let state: i32 = call!(&bus, "/", SERVER, "GetState", &())?
                    .body()
                    .deserialize()?;
                match state {
                    2 => return Ok::<(), anyhow::Error>(()),
                    4 => bail!("avahi-daemon reported a server failure"),
                    _ => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        })
        .await
        .context("avahi-daemon did not become ready within 5 seconds")??;
        Ok(bus)
    }

    async fn next(&mut self) -> Result<Message> {
        loop {
            tokio::task::yield_now().await;
            tokio::select! {
                owner = self.owners.next() => {
                    let message = owner.context("system bus closed")??;
                    let (_, old, _): (String, String, String) = message.body().deserialize()?;
                    if old == self.owner { bail!("avahi-daemon restarted or stopped"); }
                }
                signal = self.signals.next() => return signal.context("Avahi signal stream closed")?.map_err(Into::into),
            }
        }
    }
}

async fn session(operation: &mut Operation, send: &mut Ready) -> Result<()> {
    let interface = match operation {
        Operation::Register { .. } => GROUP,
        Operation::Watch { .. } => BROWSER,
    };
    let mut bus = Bus::open(interface).await?;
    let result = match operation {
        Operation::Register {
            name,
            error,
            port,
            txt,
        } => advertise(&mut bus, name, error, *port, txt, send).await,
        Operation::Watch {
            on_event,
            published,
        } => browse(&mut bus, on_event, published, send).await,
    };
    // On retry, don't retain an old client or its entry groups. On cancellation
    // the enclosing worker drops the runtime instead, with no RPC needed.
    let _ = tokio::time::timeout(Duration::from_millis(100), bus.conn.close()).await;
    result
}

fn on_path(message: &Message, path: &OwnedObjectPath) -> bool {
    message
        .header()
        .path()
        .is_some_and(|p| p.as_str() == path.as_str())
}

fn collision(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<zbus::Error>(), Some(zbus::Error::MethodError(name, _, _))
        if name.as_str() == "org.freedesktop.Avahi.CollisionError")
}

async fn advertise(
    bus: &mut Bus,
    shared_name: &Mutex<String>,
    error: &Mutex<Option<String>>,
    port: u16,
    txt: &Txt,
    send: &mut Ready,
) -> Result<()> {
    let path: OwnedObjectPath = call!(bus, "/", SERVER, "EntryGroupNew", &())?
        .body()
        .deserialize()?;
    let mut name = shared_name
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let txt: Vec<Vec<u8>> = txt
        .pairs()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}").into_bytes())
        .collect();
    let mut rename = false;
    let mut renames = 0;
    loop {
        tokio::task::yield_now().await;
        if rename {
            if renames == 16 {
                bail!("Avahi service name collided 16 times; retrying later");
            }
            renames += 1;
            call!(bus, path.as_str(), GROUP, "Reset", &())?;
            let alternative: String = call!(
                bus,
                "/",
                SERVER,
                "GetAlternativeServiceName",
                &(name.as_str(),)
            )?
            .body()
            .deserialize()?;
            if alternative.is_empty()
                || alternative.len() > MAX_NAME_BYTES
                || alternative.chars().any(char::is_control)
            {
                bail!("Avahi returned an invalid alternative service name");
            }
            name = alternative;
        }
        let added = call!(
            bus,
            path.as_str(),
            GROUP,
            "AddService",
            &(
                -1i32,
                -1i32,
                0u32,
                name.as_str(),
                SERVICE_TYPE,
                "local",
                "",
                port,
                &txt
            )
        );
        if let Err(error) = added {
            if collision(&error) {
                rename = true;
                continue;
            }
            return Err(error);
        }
        if let Err(error) = call!(bus, path.as_str(), GROUP, "Commit", &()) {
            if collision(&error) {
                rename = true;
                continue;
            }
            return Err(error);
        }
        loop {
            // Read the current state rather than interpreting a queued state
            // from before Reset. Success means established, not just queued.
            let state: i32 = call!(bus, path.as_str(), GROUP, "GetState", &())?
                .body()
                .deserialize()?;
            match state {
                2 => {
                    *shared_name.lock().unwrap_or_else(|e| e.into_inner()) = name.clone();
                    *error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    ready(send);
                }
                3 => {
                    rename = true;
                    break;
                }
                4 => bail!("Avahi entry group failed"),
                _ => {}
            }
            loop {
                let signal = bus.next().await?;
                if on_path(&signal, &path)
                    && signal
                        .header()
                        .member()
                        .is_some_and(|m| m.as_str() == "StateChanged")
                {
                    break;
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    interface: i32,
    protocol: i32,
    // Original bounded name for resolving/removing; never resolve a cleaned
    // spelling, which might refer to a different service.
    name: String,
}

fn item(message: &Message) -> Result<Option<Key>> {
    let (interface, protocol, name, service, domain, _): (i32, i32, String, String, String, u32) =
        message.body().deserialize()?;
    Ok(valid_key(interface, protocol, name, &service, &domain))
}

fn valid_key(
    interface: i32,
    protocol: i32,
    name: String,
    service: &str,
    domain: &str,
) -> Option<Key> {
    (interface > 0
        && matches!(protocol, 0 | 1)
        && !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !crate::clean(&name, MAX_NAME_BYTES).is_empty()
        && service.eq_ignore_ascii_case(SERVICE_TYPE)
        && domain.trim_end_matches('.').eq_ignore_ascii_case("local"))
    .then_some(Key {
        interface,
        protocol,
        name,
    })
}

struct Instance {
    generation: u64,
    found: [Option<Found>; 2],
    failures: [u32; 2],
    due: [Instant; 2],
    busy: [bool; 2],
}

impl Instance {
    fn new(generation: u64, now: Instant) -> Self {
        Self {
            generation,
            found: [None, None],
            failures: [0, 0],
            due: [now, now],
            busy: [false, false],
        }
    }

    fn resolved(
        &mut self,
        generation: u64,
        family: usize,
        result: Result<Found>,
        now: Instant,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        self.busy[family] = false;
        match result {
            Ok(found) => {
                self.found[family] = Some(found);
                self.failures[family] = 0;
                self.due[family] = now + RECHECK;
            }
            Err(_) => {
                self.failures[family] = self.failures[family].saturating_add(1);
                if self.failures[family] >= 2 {
                    self.found[family] = None;
                }
                // Missing IPv6 (or IPv4) is often a stable LAN property, not
                // a reason to hammer Avahi every two seconds indefinitely.
                self.due[family] = now + RECHECK.max(backoff(self.failures[family]));
            }
        }
        true
    }
}

type Instances = BTreeMap<Key, Instance>;
type Resolved = (
    i32,
    i32,
    String,
    String,
    String,
    String,
    i32,
    String,
    u16,
    Vec<Vec<u8>>,
    u32,
);

fn decode_txt(records: &[Vec<u8>]) -> Result<Txt> {
    // DNS TXT strings are at most 255 bytes each. Bound even malformed replies
    // from the local responder before converting any text or storing it.
    if records.len() > 256 || records.iter().any(|r| r.len() > 255) {
        bail!("invalid Avahi TXT lengths");
    }
    let mut txt = Txt::default();
    let mut seen = [false; 3];
    for record in records {
        let Some(split) = record.iter().position(|b| *b == b'=') else {
            continue;
        };
        let key = &record[..split];
        let index = if key.eq_ignore_ascii_case(b"id") {
            0
        } else if key.eq_ignore_ascii_case(b"proto") {
            1
        } else if key.eq_ignore_ascii_case(b"name") {
            2
        } else {
            continue;
        };
        // DNS-SD specifies the first occurrence of a duplicate key wins.
        if !seen[index] {
            txt.set(
                ["id", "proto", "name"][index],
                &String::from_utf8_lossy(&record[split + 1..]),
            );
            seen[index] = true;
        }
    }
    Ok(txt)
}

fn decoded(key: &Key, family: usize, reply: Resolved) -> Result<Found> {
    let (interface, protocol, name, service, domain, host, aprotocol, address, port, txt, _) =
        reply;
    if interface != key.interface
        || protocol != key.protocol
        || name != key.name
        || !service.eq_ignore_ascii_case(SERVICE_TYPE)
        || !domain.trim_end_matches('.').eq_ignore_ascii_case("local")
        || aprotocol != family as i32
        || port == 0
        || host.len() > 255
        || address.len() > 64
    {
        bail!("invalid Avahi service resolution");
    }
    let host = crate::clean_host(&host).to_ascii_lowercase();
    if host.is_empty() {
        bail!("empty Avahi host");
    }
    let ip: IpAddr = address.parse().context("invalid Avahi address")?;
    if ip.is_unspecified() || ip.is_multicast() || ip.is_ipv6() != (family == 1) {
        bail!("invalid Avahi address family");
    }
    let addr = match ip {
        IpAddr::V6(ip) if ip.is_unicast_link_local() => {
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, interface as u32))
        }
        _ => SocketAddr::new(ip, port),
    };
    Ok(Found {
        name: crate::clean(&name, MAX_NAME_BYTES),
        host,
        port,
        addrs: vec![addr],
        txt: decode_txt(&txt)?,
    })
}

fn same_identity(a: &Found, b: &Found) -> bool {
    a.host == b.host && a.port == b.port && a.txt == b.txt
}

fn aggregate(instances: &Instances, name: &str, previous: Option<&Found>) -> Option<Found> {
    let candidates = || {
        instances
            .iter()
            .flat_map(|(key, instance)| {
                instance
                    .found
                    .iter()
                    .flatten()
                    .map(move |found| (key, found))
            })
            .filter(move |(_, found)| found.name == name)
    };
    // Keep the visible identity stable when two LANs contain the same name.
    // Once it disappears, switch to another one, never union their addresses.
    let (key, first) = previous
        .and_then(|old| candidates().find(|(_, f)| same_identity(old, f)))
        .or_else(|| candidates().next())?;
    let mut found = first.clone();
    for (other, candidate) in candidates() {
        if other.name == key.name && same_identity(first, candidate) {
            found.addrs.extend_from_slice(&candidate.addrs);
        }
    }
    crate::sort_addrs(&mut found.addrs);
    found.addrs.truncate(MAX_ADDRESSES);
    Some(found)
}

fn publish(
    instances: &Instances,
    published: &mut BTreeMap<String, Found>,
    name: &str,
) -> Option<Event> {
    match aggregate(instances, name, published.get(name)) {
        Some(found) if published.get(name) != Some(&found) => {
            published.insert(name.to_owned(), found.clone());
            Some(Event::Found(found))
        }
        None if published.remove(name).is_some() => Some(Event::Lost {
            name: name.to_owned(),
        }),
        _ => None,
    }
}

/// Avahi can emit the entire cached browse before replying to BrowserNew.
/// Drain its bounded zbus queue while waiting: backpressure must never keep
/// the method reply behind signals we are waiting for that reply to consume.
/// Keep only a bounded current set; removals supersede buffered arrivals.
async fn drain_browser_start<T>(
    reply: impl Future<Output = Result<T>>,
    mut signals: impl Stream<Item = Result<Message>> + Unpin,
) -> Result<(T, Vec<Message>)> {
    tokio::pin!(reply);
    let mut pending = BTreeMap::new();
    loop {
        tokio::task::yield_now().await;
        tokio::select! {
            biased;
            result = &mut reply => return Ok((result?, pending.into_values().collect())),
            message = signals.next() => {
                let message = message.context("Avahi signal stream closed during browser creation")??;
                let header = message.header();
                match header.member().map(|m| m.as_str()) {
                    Some("Failure") => {
                        let error: String = message.body().deserialize()?;
                        bail!("Avahi browser failed: {}", crate::clean(&error, 255));
                    }
                    Some(member @ ("ItemNew" | "ItemRemove")) => {
                        let Some(key) = item(&message)? else { continue };
                        if member == "ItemRemove" {
                            pending.remove(&key);
                        } else if pending.contains_key(&key) || pending.len() < MAX_ENGINES {
                            pending.insert(key, message);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn browse(
    bus: &mut Bus,
    on_event: &mut OnEvent,
    published: &mut BTreeMap<String, Found>,
    send: &mut Ready,
) -> Result<()> {
    let conn = bus.conn.clone();
    let owner = bus.owner.clone();
    let create = async {
        let reply = timed(conn.call_method(
            Some(owner.as_str()),
            "/",
            Some(SERVER),
            "ServiceBrowserNew",
            &(-1i32, -1i32, SERVICE_TYPE, "local", 0u32),
        ))
        .await?;
        Ok(reply.body().deserialize::<OwnedObjectPath>()?)
    };
    let signals = Box::pin(futures_util::stream::try_unfold(&mut *bus, |bus| async {
        Ok::<_, anyhow::Error>(Some((bus.next().await?, bus)))
    }));
    let (path, pending) = drain_browser_start(create, signals).await?;
    let mut pending = pending.into_iter();
    ready(send);
    let mut instances = Instances::new();
    let mut generation = 0u64;
    // Only the connection and pinned owner are borrowed by resolve futures;
    // signal streams remain independently drainable while RPCs are in flight.
    let mut resolving = FuturesUnordered::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        // Even ignored/malformed/flooded signals must give stop a turn.
        tokio::task::yield_now().await;
        let now = Instant::now();
        // Earliest due first prevents a flood of failing names starving older
        // services. No queued work beyond the bounded instance table.
        while resolving.len() < MAX_RESOLVING {
            let next = instances
                .iter()
                .flat_map(|(key, instance)| {
                    (0..2).filter_map(move |family| {
                        (!instance.busy[family] && instance.due[family] <= now).then_some((
                            instance.due[family],
                            key.clone(),
                            family,
                        ))
                    })
                })
                .min_by(|a, b| a.0.cmp(&b.0));
            let Some((_, key, family)) = next else { break };
            let instance = instances.get_mut(&key).expect("scheduled instance exists");
            instance.busy[family] = true;
            resolving.push(resolve_call(
                &conn,
                &owner,
                key,
                instance.generation,
                family,
            ));
        }
        tokio::select! {
            signal = async {
                match pending.next() {
                    Some(signal) => Ok(signal),
                    None => bus.next().await,
                }
            } => {
                let signal = signal?;
                if !on_path(&signal, &path) { continue; }
                let header = signal.header();
                match header.member().map(|m| m.as_str()) {
                    Some("Failure") => {
                        let error: String = signal.body().deserialize()?;
                        bail!("Avahi browser failed: {}", crate::clean(&error, 255));
                    }
                    Some(member @ ("ItemNew" | "ItemRemove")) => {
                        let Some(key) = item(&signal)? else { continue };
                        let name = crate::clean(&key.name, MAX_NAME_BYTES);
                        if member == "ItemRemove" {
                            instances.remove(&key);
                            if let Some(event) = publish(&instances, published, &name) { on_event(event); }
                        } else if !instances.contains_key(&key) && instances.len() < MAX_INSTANCES {
                            let names: std::collections::BTreeSet<_> = instances.keys()
                                .map(|k| crate::clean(&k.name, MAX_NAME_BYTES)).collect();
                            if names.contains(&name) || names.len() < MAX_ENGINES {
                                generation = generation.checked_add(1).context("Avahi instance generation exhausted")?;
                                instances.insert(key, Instance::new(generation, Instant::now()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some((key, generation, family, result)) = resolving.next(), if !resolving.is_empty() => {
                let Some(instance) = instances.get_mut(&key) else { continue };
                if !instance.resolved(generation, family, result, Instant::now()) { continue; }
                let name = crate::clean(&key.name, MAX_NAME_BYTES);
                if let Some(event) = publish(&instances, published, &name) { on_event(event); }
            }
            _ = tick.tick() => {}
        }
    }
}

async fn resolve_call(
    conn: &Connection,
    owner: &str,
    key: Key,
    generation: u64,
    family: usize,
) -> (Key, u64, usize, Result<Found>) {
    let result = async {
        let reply: Resolved = timed(conn.call_method(
            Some(owner),
            "/",
            Some(SERVER),
            "ResolveService",
            &(
                key.interface,
                key.protocol,
                key.name.as_str(),
                SERVICE_TYPE,
                "local",
                family as i32,
                0u32,
            ),
        ))
        .await?
        .body()
        .deserialize()?;
        decoded(&key, family, reply)
    }
    .await;
    (key, generation, family, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_session_resets_retry_delay() {
        assert_eq!(
            backoff(retry_failures(5, None) - 1),
            Duration::from_secs(30)
        );
        assert_eq!(
            retry_failures(5, Some(HEALTHY_SESSION - Duration::from_secs(1))),
            6
        );
        assert_eq!(
            backoff(retry_failures(5, Some(HEALTHY_SESSION)) - 1),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn browser_start_drains_more_than_a_full_signal_queue() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (send, receive) = tokio::sync::mpsc::channel(SIGNAL_QUEUE);
            let signal = |member: &str, name: String| {
                Message::signal("/browser", BROWSER, member)
                    .unwrap()
                    .build(&(1i32, 0i32, name, SERVICE_TYPE, "local", 0u32))
                    .unwrap()
            };
            // Like zbus's socket reader, this producer cannot deliver the
            // method reply until all earlier signals fit in its bounded queue.
            let reply = async move {
                for i in 0..MAX_ENGINES {
                    send.send(signal("ItemNew", format!("Engine-{i:04}")))
                        .await
                        .unwrap();
                }
                send.send(signal("ItemRemove", "Engine-0000".into()))
                    .await
                    .unwrap();
                send.send(signal("ItemNew", "Replacement".into()))
                    .await
                    .unwrap();
                for i in MAX_ENGINES..(SIGNAL_QUEUE * 8) {
                    send.send(signal("ItemNew", format!("Engine-{i:04}")))
                        .await
                        .unwrap();
                }
                Ok("method reply")
            };
            let signals = Box::pin(futures_util::stream::unfold(receive, |mut receive| async {
                receive.recv().await.map(|message| (Ok(message), receive))
            }));
            let (reply, pending) =
                tokio::time::timeout(RPC_TIMEOUT, drain_browser_start(reply, signals))
                    .await
                    .expect("method reply starved behind a full signal queue")
                    .unwrap();
            assert_eq!(reply, "method reply");
            assert_eq!(pending.len(), MAX_ENGINES);
            let names: Vec<_> = pending
                .iter()
                .map(|message| item(message).unwrap().unwrap().name)
                .collect();
            assert!(names.iter().any(|name| name == "Replacement"));
            assert!(!names.iter().any(|name| name == "Engine-0000"));
        });
    }

    fn key(interface: i32) -> Key {
        Key {
            interface,
            protocol: 0,
            name: "Engine".into(),
        }
    }

    fn found(key: &Key, family: usize, address: &str, id: &str) -> Found {
        decoded(
            key,
            family,
            (
                key.interface,
                key.protocol,
                key.name.clone(),
                SERVICE_TYPE.into(),
                "local".into(),
                "HOST.local.".into(),
                family as i32,
                address.into(),
                47470,
                vec![format!("id={id}").into_bytes()],
                0,
            ),
        )
        .unwrap()
    }

    #[test]
    fn txt_is_bounded_cleaned_and_first_key_wins() {
        let txt = decode_txt(&[
            b"ID=first\n".to_vec(),
            b"id=second".to_vec(),
            b"unknown=x".to_vec(),
            [b"name=".as_slice(), &[b'x'; 240]].concat(),
            b"proto=1".to_vec(),
        ])
        .unwrap();
        assert_eq!(txt.id, "first");
        assert_eq!(txt.name.len(), crate::MAX_TXT_VALUE_BYTES);
        assert_eq!(txt.proto, "1");
        assert!(decode_txt(&[vec![b'x'; 256]]).is_err());
        assert!(decode_txt(&vec![vec![]; 257]).is_err());
    }

    #[test]
    fn names_and_addresses_are_validated_and_scoped() {
        assert!(valid_key(1, 0, "x".repeat(64), SERVICE_TYPE, "local").is_none());
        assert!(valid_key(-1, 0, "x".into(), SERVICE_TYPE, "local").is_none());
        assert!(valid_key(1, 0, "\n".into(), SERVICE_TYPE, "local").is_none());
        let found = found(&key(7), 1, "fe80::1", "id");
        assert_eq!(found.host, "host.local");
        assert_eq!(
            found.addrs[0],
            "[fe80::1%7]:47470".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn removal_keeps_other_interfaces_and_never_merges_identities() {
        let mut instances = Instances::new();
        for (interface, id) in [(1, "a"), (2, "a"), (3, "different")] {
            let key = key(interface);
            let mut instance = Instance::new(interface as u64, Instant::now());
            instance.found[0] = Some(found(&key, 0, &format!("192.168.1.{interface}"), id));
            instance.found[1] = Some(found(&key, 1, "fe80::1", id));
            instances.insert(key, instance);
        }
        let mut published = BTreeMap::new();
        assert!(matches!(
            publish(&instances, &mut published, "Engine"),
            Some(Event::Found(_))
        ));
        assert_eq!(published["Engine"].addrs.len(), 4);
        assert!(published["Engine"].addrs[0].is_ipv4());
        instances.remove(&key(1));
        assert!(matches!(
            publish(&instances, &mut published, "Engine"),
            Some(Event::Found(_))
        ));
        assert_eq!(published["Engine"].addrs.len(), 2);
        assert_eq!(published["Engine"].txt.id, "a");
        instances.remove(&key(2));
        assert!(matches!(
            publish(&instances, &mut published, "Engine"),
            Some(Event::Found(_))
        ));
        assert_eq!(published["Engine"].txt.id, "different");
        instances.clear();
        assert!(matches!(
            publish(&instances, &mut published, "Engine"),
            Some(Event::Lost { .. })
        ));
        assert!(publish(&instances, &mut published, "Engine").is_none());
    }

    #[test]
    fn stale_resolutions_do_not_resurrect_and_two_failures_remove_a_family() {
        let now = Instant::now();
        let mut instance = Instance::new(2, now);
        let found = found(&key(1), 0, "192.168.1.1", "id");
        assert!(!instance.resolved(1, 0, Ok(found.clone()), now));
        assert!(instance.found[0].is_none());
        assert!(instance.resolved(2, 0, Ok(found.clone()), now));
        assert_eq!(instance.due[0], now + RECHECK);
        instance.resolved(2, 0, Err(anyhow!("timeout")), now);
        assert!(instance.due[0] >= now + RECHECK);
        assert_eq!(instance.found[0], Some(found));
        instance.resolved(2, 0, Err(anyhow!("timeout")), now);
        assert!(instance.found[0].is_none());
        assert_eq!(instance.failures[0], 2);
        assert!(instance.due[0] >= now + RECHECK);
    }

    #[test]
    fn aggregate_address_list_is_bounded() {
        let mut instances = Instances::new();
        for interface in 1..=32 {
            let key = key(interface);
            let mut instance = Instance::new(interface as u64, Instant::now());
            instance.found[0] = Some(found(&key, 0, &format!("192.168.1.{interface}"), "same"));
            instances.insert(key, instance);
        }
        assert_eq!(
            aggregate(&instances, "Engine", None).unwrap().addrs.len(),
            MAX_ADDRESSES
        );
    }
}
