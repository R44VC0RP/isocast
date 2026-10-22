//! The Windows DNS-SD API (`DnsServiceRegister`/`Browse`/`Resolve`,
//! dnsapi.dll, Windows 10 1809+). The system's DNS Client service runs
//! mDNS: it parses the packets and owns UDP 5353 (with its own built-in
//! firewall rule), so the engine needs none.
//!
//! Callbacks run on system threadpool threads, possibly after a cancel
//! returns. Their context is therefore not a pointer to our state but a
//! token looked up in [`ROUTES`]. Microsoft documents only that a cancel
//! handle must stay valid until the operation is cancelled, not when the
//! system stops reading a request, so every request, its strings and its
//! cancel handle are owned by its route and freed only after the
//! operation's final callback (a completion, or the cancel's
//! `ERROR_CANCELLED`) has arrived and its owner has let go. An operation
//! whose final callback never comes keeps its few bytes (a bounded leak,
//! never a dangling read).
//!
//! A running browse is told when a service says goodbye (a PTR answer with
//! TTL 0), but a new browse is first answered from the DNS Client's cache,
//! which keeps PTR and SRV records of engines that are long gone (seen on
//! Windows 10 22H2: up to their 75-minute TTL, even after a goodbye). So
//! a browse answer only says where to look: an engine is reported once a
//! resolve, which really asks the network, answers, and checked again
//! every [`RECHECK`]. Two failed checks in a row report it lost. A
//! running browse doesn't report engines that start after it, so it is
//! restarted every [`BROWSE_RESTART`].

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{DNS_REQUEST_PENDING, ERROR_CANCELLED, ERROR_SUCCESS};
use windows_sys::Win32::NetworkManagement::Dns::{
    DNS_QUERY_REQUEST_VERSION1, DNS_RECORDW, DNS_SERVICE_BROWSE_REQUEST, DNS_SERVICE_CANCEL,
    DNS_SERVICE_INSTANCE, DNS_SERVICE_REGISTER_REQUEST, DNS_SERVICE_RESOLVE_REQUEST, DNS_TYPE_A,
    DNS_TYPE_AAAA, DNS_TYPE_PTR, DnsFree, DnsFreeRecordList, DnsServiceBrowse,
    DnsServiceBrowseCancel, DnsServiceConstructInstance, DnsServiceDeRegister,
    DnsServiceFreeInstance, DnsServiceRegister, DnsServiceRegisterCancel, DnsServiceResolve,
    DnsServiceResolveCancel,
};
use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};

use crate::{Event, Found, MAX_ENGINES, OnEvent, SERVICE_TYPE, Txt};

pub const SUPPORTED: bool = true;

/// How often a known engine is resolved again, to confirm it still
/// answers and follow address and TXT changes.
const RECHECK: Duration = Duration::from_secs(30);
/// How long one resolve may take before it counts as failed.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Failed resolves in a row that report an engine lost, and that forget
/// it entirely (until a browse answers for it again).
const LOST_AFTER: u32 = 2;
const FORGET_AFTER: u32 = 10;
/// How often the browse is restarted (known engines are kept). A running
/// browse reports goodbyes but not engines that start after it (seen on
/// Windows 10 22H2), so new engines are found by the next browse.
const BROWSE_RESTART: Duration = Duration::from_secs(10);
/// How long an instance forgotten for not answering is ignored when the
/// cache offers it again.
const IGNORE_FORGOTTEN: Duration = Duration::from_secs(300);
/// How long registering may take before it is cancelled.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);
/// Browse answers this long after the browse started came from the
/// network, not the cache (which answers at once: within milliseconds on
/// exon-desk, the network after about a second).
const HEARD_AFTER: Duration = Duration::from_millis(300);
/// Resolves running at once.
const MAX_RESOLVING: usize = 4;
/// Messages applied per loop, so stopping stays prompt in a flood.
const DRAIN_BUDGET: usize = 64;
/// Longest TTL an address from a browse answer is kept for.
const MAX_ADDRESS_TTL: u32 = 4500;
const ADDRS_PER_HOST: usize = 16;

/// Delay before the next resolve after `failures` failures in a row.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.min(5)).min(30))
}

/// `_isocast._tcp.local`, the name browsed and the suffix of instances.
fn service_domain() -> String {
    format!("{SERVICE_TYPE}.local")
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// A NUL-terminated UTF-16 string from the system, bounded.
fn from_wide(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    // SAFETY: the system passes NUL-terminated strings; reading stops at
    // the NUL or after 1024 units.
    while len < 1024 && unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// The instance name in a full service name (`Name._isocast._tcp.local`).
fn instance_of(full: &str) -> Option<String> {
    let suffix = format!(".{}", service_domain());
    let full = full.trim_end_matches('.');
    let cut = full.len().checked_sub(suffix.len())?;
    (full.is_char_boundary(cut) && full[cut..].eq_ignore_ascii_case(&suffix))
        .then(|| crate::clean(&full[..cut], 255))
        .filter(|name| !name.is_empty())
}

// ---------------------------------------------------------------- routing

enum Msg {
    Registered {
        status: u32,
        instance: Instance,
    },
    Browsed {
        token: usize,
        status: u32,
        /// PTR answers: the full instance name and its TTL (0: goodbye).
        answers: Vec<(String, u32)>,
        /// A and AAAA answers: a host, one of its addresses, and the TTL.
        hosts: Vec<(String, IpAddr, u32)>,
        /// When the callback ran.
        at: Instant,
    },
    Resolved {
        token: usize,
        status: u32,
        instance: Instance,
    },
}

/// Memory the system may still read (a request, its strings, a cancel
/// handle), kept until it is done with it.
struct Keep(#[allow(dead_code)] Box<dyn std::any::Any>);

// SAFETY: plain allocations (raw pointers into each other), only moved
// between threads and freed, never used through this wrapper.
unsafe impl Send for Keep {}

struct Route {
    /// Where callbacks deliver; `None` once the owner let go.
    sender: Option<SyncSender<Msg>>,
    /// Only held (freed with the route).
    _keep: Option<Keep>,
    /// The final callback arrived: the system won't read `keep` again.
    finished: bool,
    /// The owner is inside a cancel call that uses `keep`.
    cancelling: bool,
}

static ROUTES: Mutex<BTreeMap<usize, Route>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicUsize = AtomicUsize::new(1);

fn routes() -> std::sync::MutexGuard<'static, BTreeMap<usize, Route>> {
    ROUTES.lock().unwrap_or_else(|e| e.into_inner())
}

fn new_token() -> usize {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Operations started and not yet finished by the system, at most; and
/// of those, ones whose owner let go (cancelled, final callback still
/// due). Past either, no new operation starts, so callbacks the system
/// never sends can't grow memory without bound.
const MAX_ROUTES: usize = 256;
const MAX_ABANDONED: usize = 64;

/// Register a new operation, or why it can't start (the budget is spent;
/// `keep` is then dropped, the system never having seen it).
fn add_route(token: usize, sender: SyncSender<Msg>, keep: Option<Keep>) -> Result<(), String> {
    let mut routes = routes();
    let abandoned = routes.values().filter(|r| r.sender.is_none()).count();
    if routes.len() >= MAX_ROUTES || abandoned >= MAX_ABANDONED {
        return Err(format!(
            "discovery is paused: {} DNS-SD operations are still unfinished in Windows \
             ({abandoned} abandoned)",
            routes.len()
        ));
    }
    routes.insert(
        token,
        Route {
            sender: Some(sender),
            _keep: keep,
            finished: false,
            cancelling: false,
        },
    );
    Ok(())
}

/// Forget a route whose operation the system never started, or whose
/// owner knows it is over.
fn end_route(token: usize) {
    let removed = routes().remove(&token);
    drop(removed);
}

/// Deliver to the operation `token`, if its owner still listens. Never
/// blocks: a full queue drops the message (and frees what it holds).
/// `last`: this is the operation's final callback.
fn deliver(token: usize, msg: Msg, last: bool) {
    let mut freed = None;
    let sender = {
        let mut routes = routes();
        let Some(route) = routes.get_mut(&token) else {
            return;
        };
        if last {
            route.finished = true;
        }
        let sender = route.sender.clone();
        if last && sender.is_none() && !route.cancelling {
            freed = routes.remove(&token);
        }
        sender
    };
    drop(freed);
    if let Some(sender) = sender {
        let _ = sender.try_send(msg);
    }
}

/// A `DNS_SERVICE_INSTANCE` the system handed us, freed on drop.
struct Instance(*mut DNS_SERVICE_INSTANCE);

// SAFETY: an owned allocation, not tied to the thread that received it.
unsafe impl Send for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by dnsapi, freed exactly once.
            unsafe { DnsServiceFreeInstance(self.0) }
        }
    }
}

impl Instance {
    fn get(&self) -> Option<&DNS_SERVICE_INSTANCE> {
        // SAFETY: null or a live instance we own.
        unsafe { self.0.as_ref() }
    }
}

// ---------------------------------------------------------------- register

unsafe extern "system" fn on_registered(
    status: u32,
    context: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    deliver(
        context as usize,
        Msg::Registered {
            status,
            instance: Instance(instance.cast_mut()),
        },
        false,
    );
}

/// What a registration keeps alive for the system: the request (also
/// used to deregister), the instance it was built from, the strings that
/// instance was built from and the cancel handle.
struct Inputs {
    request: Box<DNS_SERVICE_REGISTER_REQUEST>,
    _constructed: Instance,
    _strings: Vec<Vec<u16>>,
    cancel: Box<DNS_SERVICE_CANCEL>,
}

// SAFETY: allocations owned by the registration; the system only reads them.
unsafe impl Send for Inputs {}

pub struct Registration {
    token: usize,
    receive: Receiver<Msg>,
    inputs: Option<Inputs>,
    registered: Instance,
    name: String,
}

impl Registration {
    pub fn name(&self) -> String {
        // Windows reports renames only when registering.
        self.name.clone()
    }
}

/// Ask the system to withdraw `registered`, registered with `inputs`.
/// Whether the request is pending (its completion comes as a
/// `Msg::Registered`); `false` means it was refused at once.
fn request_deregister(inputs: &mut Inputs, registered: &Instance) -> bool {
    // Deregistering takes the request it was registered with, carrying the
    // instance the system confirmed.
    inputs.request.pServiceInstance = registered.0;
    // SAFETY: the request and instance stay alive until the completion
    // (held by the registration or the cleanup worker until then).
    let status = unsafe { DnsServiceDeRegister(&*inputs.request, std::ptr::null_mut()) };
    if status != DNS_REQUEST_PENDING as u32 {
        isocast_diagnostics::diagnostic!("warn"; "discovery: could not stop advertising (DnsServiceDeRegister {status})");
        return false;
    }
    true
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Some(mut inputs) = self.inputs.take() else {
            return;
        };
        let registered = std::mem::replace(&mut self.registered, Instance(std::ptr::null_mut()));
        if !request_deregister(&mut inputs, &registered) {
            // Refused: the system won't call back, nor keep using them.
            end_route(self.token);
            return;
        }
        if let Ok(Msg::Registered { .. }) = self.receive.recv_timeout(Duration::from_secs(3)) {
            end_route(self.token);
            return;
        }
        // Not confirmed yet: the cleanup worker keeps everything alive
        // until the completion arrives.
        let receive = std::mem::replace(&mut self.receive, mpsc::sync_channel(1).1);
        hand_over(Pending {
            token: self.token,
            receive,
            inputs,
            state: PendingState::Deregistering(registered),
            since: Instant::now(),
        });
    }
}

// ---------------------------------------------------------------- cleanup

/// A registration the system hasn't finished with.
struct Pending {
    token: usize,
    receive: Receiver<Msg>,
    inputs: Inputs,
    state: PendingState,
    since: Instant,
}

enum PendingState {
    /// Cancelled after missing its confirmation; a success may still
    /// arrive, and must then be withdrawn.
    Cancelling,
    /// Withdrawal requested; waiting for its completion. Holds the
    /// confirmed instance the request points at until then.
    Deregistering(#[allow(dead_code)] Instance),
}

static CLEANUP: OnceLock<Option<Mutex<mpsc::Sender<Pending>>>> = OnceLock::new();

/// The process's one cleanup worker: it owns every registration the system
/// hasn't finished with (their memory and the duty to withdraw a late
/// success) until the system's completion arrives, however long that takes.
/// `None` if it couldn't be started.
fn cleanup() -> Option<&'static Mutex<mpsc::Sender<Pending>>> {
    CLEANUP
        .get_or_init(|| {
            let (send, receive) = mpsc::channel::<Pending>();
            std::thread::Builder::new()
                .name("isocast-discovery-cleanup".into())
                .spawn(move || run_cleanup(receive))
                .map_err(|error| {
                    isocast_diagnostics::diagnostic!("warn"; "discovery: could not start the cleanup worker: {error}");
                })
                .ok()
                .map(|_| Mutex::new(send))
        })
        .as_ref()
}

/// Give a pending registration to the cleanup worker. It exists (checked
/// before registering); should sending ever fail, the memory is kept
/// forever rather than freed under the system.
fn hand_over(pending: Pending) {
    let unsent = match cleanup() {
        Some(send) => send
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(pending)
            .err()
            .map(|e| e.0),
        None => Some(pending),
    };
    if let Some(pending) = unsent {
        std::mem::forget(pending);
    }
}

fn run_cleanup(new: Receiver<Pending>) {
    let mut jobs: Vec<Pending> = Vec::new();
    loop {
        let incoming = if jobs.is_empty() {
            new.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            new.recv_timeout(Duration::from_millis(100))
        };
        match incoming {
            Ok(job) => jobs.push(job),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) if jobs.is_empty() => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
        jobs = jobs.into_iter().filter_map(advance).collect();
    }
}

/// Move one pending registration on; `None` once the system is done.
fn advance(mut job: Pending) -> Option<Pending> {
    let Ok(Msg::Registered { status, instance }) = job.receive.try_recv() else {
        if job.since.elapsed() > Duration::from_secs(60) {
            isocast_diagnostics::diagnostic!("warn"; "discovery: a registration is still waiting for Windows after 60 s; still holding it");
            job.since = Instant::now();
        }
        return Some(job);
    };
    match job.state {
        PendingState::Cancelling if status == ERROR_SUCCESS && !instance.0.is_null() => {
            isocast_diagnostics::diagnostic!("info"; "discovery: a late registration completed; withdrawing it");
            if request_deregister(&mut job.inputs, &instance) {
                job.state = PendingState::Deregistering(instance);
                job.since = Instant::now();
                return Some(job);
            }
            end_route(job.token);
            None
        }
        // Cancelled, failed, or withdrawn: the system is done with it.
        _ => {
            end_route(job.token);
            None
        }
    }
}

fn computer_name() -> Result<String> {
    let mut buffer = vec![0u16; 256];
    let mut size = buffer.len() as u32;
    // SAFETY: `buffer` holds `size` units.
    let ok = unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut size) };
    if ok == 0 {
        bail!(
            "could not read this computer's host name: {}",
            std::io::Error::last_os_error()
        );
    }
    buffer.truncate(size as usize);
    Ok(String::from_utf16_lossy(&buffer))
}

pub fn register(name: &str, port: u16, txt: &Txt) -> Result<Registration> {
    let service = wide(&format!("{name}.{}", service_domain()));
    let host = wide(&format!("{}.local", computer_name()?));
    let keys: Vec<Vec<u16>> = txt.pairs().iter().map(|(key, _)| wide(key)).collect();
    let values: Vec<Vec<u16>> = txt.pairs().iter().map(|(_, value)| wide(value)).collect();
    let key_ptrs: Vec<*const u16> = keys.iter().map(|k| k.as_ptr()).collect();
    let value_ptrs: Vec<*const u16> = values.iter().map(|v| v.as_ptr()).collect();
    // No addresses: the DNS Client answers for this computer's own host
    // name on every interface, so the record follows address changes.
    // SAFETY: valid NUL-terminated strings and arrays of `len` entries.
    let constructed = Instance(unsafe {
        DnsServiceConstructInstance(
            service.as_ptr(),
            host.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            port,
            0,
            0,
            key_ptrs.len() as u32,
            key_ptrs.as_ptr(),
            value_ptrs.as_ptr(),
        )
    });
    if constructed.0.is_null() {
        bail!("could not advertise {name}: DnsServiceConstructInstance failed");
    }
    let (send, receive) = mpsc::sync_channel(4);
    let token = new_token();
    let mut inputs = Inputs {
        request: Box::new(DNS_SERVICE_REGISTER_REQUEST {
            Version: DNS_QUERY_REQUEST_VERSION1,
            InterfaceIndex: 0,
            pServiceInstance: constructed.0,
            pRegisterCompletionCallback: Some(on_registered),
            pQueryContext: token as *mut c_void,
            hCredentials: std::ptr::null_mut(),
            unicastEnabled: 0,
        }),
        _constructed: constructed,
        _strings: [vec![service, host], keys, values].concat(),
        cancel: Box::default(),
    };
    // Whatever the system doesn't finish in time goes to the cleanup
    // worker, so it must exist before anything is pending.
    if cleanup().is_none() {
        bail!("could not advertise {name}: the discovery cleanup worker couldn't start");
    }
    if let Err(reason) = add_route(token, send, None) {
        bail!("could not advertise {name}: {reason}");
    }
    // SAFETY: everything the request points at lives in `inputs`, which is
    // kept until the system is done with it (see `Registration`, the
    // cleanup worker).
    let status = unsafe { DnsServiceRegister(&*inputs.request, &mut *inputs.cancel) };
    if status != DNS_REQUEST_PENDING as u32 {
        end_route(token);
        bail!("could not advertise {name}: DnsServiceRegister error {status}");
    }
    match receive.recv_timeout(REGISTER_TIMEOUT) {
        Ok(Msg::Registered { status, instance }) if status == ERROR_SUCCESS => {
            if instance.0.is_null() {
                end_route(token);
                bail!("could not advertise {name}: the system confirmed no service");
            }
            let actual = instance
                .get()
                .map(|i| from_wide(i.pszInstanceName))
                .and_then(|full| instance_of(&full))
                .unwrap_or_else(|| name.to_owned());
            Ok(Registration {
                token,
                receive,
                inputs: Some(inputs),
                registered: instance,
                name: actual,
            })
        }
        Ok(Msg::Registered { status, .. }) => {
            end_route(token);
            bail!("could not advertise {name}: DNS-SD registration failed with error {status}")
        }
        _ => {
            // SAFETY: the handle from our pending DnsServiceRegister, alive
            // in `inputs`.
            unsafe { DnsServiceRegisterCancel(&*inputs.cancel) };
            hand_over(Pending {
                token,
                receive,
                inputs,
                state: PendingState::Cancelling,
                since: Instant::now(),
            });
            bail!(
                "could not advertise {name}: the system didn't confirm within {} s",
                REGISTER_TIMEOUT.as_secs()
            )
        }
    }
}

// ---------------------------------------------------------------- watch

unsafe extern "system" fn on_browsed(
    status: u32,
    context: *const c_void,
    records: *const DNS_RECORDW,
) {
    let mut answers = Vec::new();
    let mut hosts = Vec::new();
    let mut record = records;
    let mut count = 0;
    // SAFETY: a list of records owned by us from here on; read, then freed.
    while let Some(r) = unsafe { record.as_ref() } {
        if count < 256 {
            count += 1;
            // SAFETY: the data union's member for the record's type.
            match r.wType {
                DNS_TYPE_PTR => {
                    answers.push((from_wide(unsafe { r.Data.PTR.pNameHost }), r.dwTtl));
                }
                DNS_TYPE_A => {
                    let ip = Ipv4Addr::from(unsafe { r.Data.A.IpAddress }.to_ne_bytes());
                    hosts.push((host_key(&from_wide(r.pName)), IpAddr::V4(ip), r.dwTtl));
                }
                DNS_TYPE_AAAA => {
                    let ip = Ipv6Addr::from(unsafe { r.Data.AAAA.Ip6Address.IP6Byte });
                    hosts.push((host_key(&from_wide(r.pName)), IpAddr::V6(ip), r.dwTtl));
                }
                _ => {}
            }
        }
        record = r.pNext;
    }
    if !records.is_null() {
        // SAFETY: the record list this callback was given.
        unsafe { DnsFree(records.cast(), DnsFreeRecordList) };
    }
    deliver(
        context as usize,
        Msg::Browsed {
            token: context as usize,
            status,
            answers,
            hosts,
            at: Instant::now(),
        },
        // A browse ends with an error status (its cancel's ERROR_CANCELLED
        // included); answers keep coming until then.
        status != ERROR_SUCCESS,
    );
}

/// How hosts are compared: lowercase, without the trailing dot.
fn host_key(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

unsafe extern "system" fn on_resolved(
    status: u32,
    context: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    deliver(
        context as usize,
        Msg::Resolved {
            token: context as usize,
            status,
            instance: Instance(instance.cast_mut()),
        },
        // A resolve calls back once, completed or cancelled.
        true,
    );
}

/// What cancels a running browse or resolve.
#[derive(Clone, Copy)]
enum Kind {
    Browse,
    Resolve,
}

/// A running browse or resolve. Its request, name and cancel handle live
/// in its route; dropping it cancels it unless it already finished.
struct Query {
    token: usize,
    kind: Kind,
    cancel: *const DNS_SERVICE_CANCEL,
}

// SAFETY: `cancel` points into the route's kept memory, freed only after
// the owner (this query) let go; the query is owned by one thread.
unsafe impl Send for Query {}

impl Drop for Query {
    fn drop(&mut self) {
        {
            let mut routes = routes();
            let Some(route) = routes.get_mut(&self.token) else {
                return;
            };
            route.sender = None;
            if route.finished {
                let removed = routes.remove(&self.token);
                drop(routes);
                drop(removed);
                return;
            }
            route.cancelling = true;
        }
        // SAFETY: the handle filled by our pending request; its memory
        // stays in the route while `cancelling` is set.
        unsafe {
            match self.kind {
                Kind::Browse => {
                    DnsServiceBrowseCancel(self.cancel);
                }
                Kind::Resolve => {
                    DnsServiceResolveCancel(self.cancel);
                }
            }
        }
        let mut routes = routes();
        let finished = routes.get_mut(&self.token).is_some_and(|route| {
            route.cancelling = false;
            route.finished
        });
        // Otherwise the final callback frees it when it comes.
        if finished {
            let removed = routes.remove(&self.token);
            drop(routes);
            drop(removed);
        }
    }
}

fn start_browse(send: &SyncSender<Msg>) -> Result<Query> {
    let name = wide(&service_domain());
    let token = new_token();
    let mut request = Box::new(DNS_SERVICE_BROWSE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1,
        InterfaceIndex: 0,
        QueryName: name.as_ptr(),
        pQueryContext: token as *mut c_void,
        ..Default::default()
    });
    request.Anonymous.pBrowseCallback = Some(on_browsed);
    let mut cancel = Box::new(DNS_SERVICE_CANCEL::default());
    let request_ptr: *const DNS_SERVICE_BROWSE_REQUEST = &*request;
    let cancel_ptr: *mut DNS_SERVICE_CANCEL = &mut *cancel;
    if let Err(reason) = add_route(
        token,
        send.clone(),
        Some(Keep(Box::new((name, request, cancel)))),
    ) {
        bail!("could not browse for engines: {reason}");
    }
    // SAFETY: the request, its name and the cancel handle live in the
    // route until the browse's final callback (boxes don't move).
    let status = unsafe { DnsServiceBrowse(request_ptr, cancel_ptr) };
    if status != DNS_REQUEST_PENDING {
        end_route(token);
        bail!("could not browse for engines: DnsServiceBrowse error {status}");
    }
    Ok(Query {
        token,
        kind: Kind::Browse,
        cancel: cancel_ptr,
    })
}

/// Why a resolve didn't start.
enum Refused {
    /// The outstanding-operation budget is spent (see [`MAX_ROUTES`]).
    Budget(String),
    /// The system refused it.
    System,
}

fn start_resolve(send: &SyncSender<Msg>, full: &str) -> Result<Query, Refused> {
    let mut name = wide(full);
    let token = new_token();
    let request = Box::new(DNS_SERVICE_RESOLVE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1,
        InterfaceIndex: 0,
        QueryName: name.as_mut_ptr(),
        pResolveCompletionCallback: Some(on_resolved),
        pQueryContext: token as *mut c_void,
    });
    let mut cancel = Box::new(DNS_SERVICE_CANCEL::default());
    let request_ptr: *const DNS_SERVICE_RESOLVE_REQUEST = &*request;
    let cancel_ptr: *mut DNS_SERVICE_CANCEL = &mut *cancel;
    add_route(
        token,
        send.clone(),
        Some(Keep(Box::new((name, request, cancel)))),
    )
    .map_err(Refused::Budget)?;
    // SAFETY: as in `start_browse`.
    let status = unsafe { DnsServiceResolve(request_ptr, cancel_ptr) };
    if status != DNS_REQUEST_PENDING {
        end_route(token);
        return Err(Refused::System);
    }
    Ok(Query {
        token,
        kind: Kind::Resolve,
        cancel: cancel_ptr,
    })
}

/// A resolved instance as plain data.
fn found_from(instance: &DNS_SERVICE_INSTANCE, name: &str) -> Found {
    let mut addrs = Vec::new();
    // SAFETY: null or valid for the instance's lifetime.
    if let Some(ip4) = unsafe { instance.ip4Address.as_ref() } {
        // IP4_ADDRESS keeps the address's bytes in network order.
        let ip = Ipv4Addr::from(ip4.to_ne_bytes());
        addrs.push(SocketAddr::V4(SocketAddrV4::new(ip, instance.wPort)));
    }
    if let Some(ip6) = unsafe { instance.ip6Address.as_ref() } {
        let ip = Ipv6Addr::from(unsafe { ip6.IP6Byte });
        let scope = if ip.is_unicast_link_local() {
            instance.dwInterfaceIndex
        } else {
            0
        };
        addrs.push(SocketAddr::V6(SocketAddrV6::new(
            ip,
            instance.wPort,
            0,
            scope,
        )));
    }
    let mut txt = Txt::default();
    let count = (instance.dwPropertyCount as usize).min(32);
    if !instance.keys.is_null() && !instance.values.is_null() {
        for i in 0..count {
            // SAFETY: `dwPropertyCount` entries in each array.
            let (key, value) = unsafe { (*instance.keys.add(i), *instance.values.add(i)) };
            txt.set(&from_wide(key), &from_wide(value));
        }
    }
    Found {
        name: name.to_owned(),
        host: crate::clean_host(&from_wide(instance.pszHostName)),
        port: instance.wPort,
        addrs,
        txt,
    }
}

pub struct Watch {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn watch(on_event: OnEvent) -> Result<Watch> {
    // Each answered instance is its own callback, and the cache answers a
    // new browse with every instance it holds at once.
    let (send, receive) = mpsc::sync_channel(1024);
    // Fail here, not on the thread, if browsing can't start at all.
    let browse = start_browse(&send)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread = std::thread::Builder::new()
        .name("isocast-discovery".into())
        .spawn({
            let stop = stop.clone();
            move || {
                Browser {
                    on_event,
                    send,
                    receive,
                    browse: Some((browse, Instant::now())),
                    retry_browse_at: None,
                    instances: BTreeMap::new(),
                    resolving: BTreeMap::new(),
                    hosts: BTreeMap::new(),
                    reported: BTreeMap::new(),
                    last_error: None,
                    forgotten: BTreeMap::new(),
                }
                .run(&stop)
            }
        })
        .context("could not start the discovery thread")?;
    Ok(Watch {
        stop,
        thread: Some(thread),
    })
}

struct Seen {
    full: String,
    /// The TTL of its latest browse answer.
    ttl: u32,
    /// When it was last heard on the network rather than from the cache.
    heard: Option<Instant>,
    /// The last successful resolve.
    resolved: Option<Found>,
    /// Failed resolves (started, completed or timed out) since the last
    /// success.
    failures: u32,
    resolving: Option<(Query, Instant)>,
    /// When the last resolve was attempted.
    checked: Option<Instant>,
}

impl Seen {
    fn heard_lately(&self) -> bool {
        self.heard
            .is_some_and(|at| at.elapsed() < BROWSE_RESTART * 3)
    }

    fn due(&self, now: Instant) -> bool {
        let wait = if self.failures == 0 {
            RECHECK
        } else {
            backoff(self.failures)
        };
        self.resolving.is_none() && self.checked.is_none_or(|at| now - at >= wait)
    }
}

struct Browser {
    on_event: OnEvent,
    send: SyncSender<Msg>,
    receive: Receiver<Msg>,
    browse: Option<(Query, Instant)>,
    retry_browse_at: Option<Instant>,
    /// By instance name.
    instances: BTreeMap<String, Seen>,
    /// Resolve tokens to instance names.
    resolving: BTreeMap<usize, String>,
    /// Hosts' addresses from browse answers, with when each expires. A
    /// resolve carries at most one IPv4 and one IPv6 address, and
    /// sometimes none; these fill in.
    hosts: BTreeMap<String, BTreeMap<IpAddr, Instant>>,
    reported: BTreeMap<String, Found>,
    last_error: Option<String>,
    /// Instances forgotten for not answering, and when: the cache keeps
    /// offering them, and they aren't tried again for a while.
    forgotten: BTreeMap<String, Instant>,
}

impl Browser {
    fn run(mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            let restart = match &self.browse {
                Some((_, at)) => now - *at >= BROWSE_RESTART,
                None => self.retry_browse_at.is_none_or(|at| now >= at),
            };
            if restart {
                // The old browse is cancelled before the new one starts.
                self.browse = None;
                match start_browse(&self.send) {
                    Ok(query) => {
                        self.browse = Some((query, now));
                        self.retry_browse_at = None;
                    }
                    Err(error) => {
                        self.error(format!("{error:#}"));
                        self.retry_browse_at = Some(now + Duration::from_secs(5));
                    }
                }
            }
            if let Ok(msg) = self.receive.recv_timeout(Duration::from_millis(200)) {
                self.apply(msg);
                for _ in 1..DRAIN_BUDGET {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(msg) = self.receive.try_recv() else {
                        break;
                    };
                    self.apply(msg);
                }
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            self.check();
            self.report();
        }
    }

    /// Say a browsing failure once (until browsing works again).
    fn error(&mut self, reason: String) {
        if self.last_error.as_ref() != Some(&reason) {
            self.last_error = Some(reason.clone());
            (self.on_event)(Event::Error(reason));
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Browsed {
                token,
                status,
                answers,
                hosts,
                at,
            } => {
                let Some((query, started)) = &self.browse else {
                    return;
                };
                if query.token != token {
                    return;
                }
                // The cache answers a new browse at once; anything later
                // was heard on the network (a reply to the browse's query
                // or an announcement).
                let heard = at.saturating_duration_since(*started) >= HEARD_AFTER;
                if status != ERROR_SUCCESS {
                    // The browse ended (not by us): start another soon.
                    self.browse = None;
                    self.retry_browse_at = Some(Instant::now() + Duration::from_secs(5));
                    if status != ERROR_CANCELLED {
                        self.error(format!("browsing stopped with error {status}"));
                    }
                    return;
                }
                self.last_error = None;
                self.learn_hosts(hosts);
                for (full, ttl) in answers {
                    let Some(name) = instance_of(&full) else {
                        continue;
                    };
                    if ttl == 0 {
                        self.forget(&name);
                        self.forgotten.remove(&name);
                        continue;
                    }
                    if self.forgotten.contains_key(&name) {
                        continue;
                    }
                    if !self.instances.contains_key(&name)
                        && self.instances.len() >= MAX_ENGINES
                        && !self.make_room(heard)
                    {
                        continue;
                    }
                    let seen = self.instances.entry(name).or_insert(Seen {
                        full: full.clone(),
                        ttl,
                        heard: None,
                        resolved: None,
                        failures: 0,
                        resolving: None,
                        checked: None,
                    });
                    seen.full = full;
                    seen.ttl = ttl;
                    if heard {
                        seen.heard = Some(at);
                    }
                }
            }
            Msg::Resolved {
                token,
                status,
                instance,
            } => {
                let Some(name) = self.resolving.remove(&token) else {
                    return;
                };
                let Some(seen) = self.instances.get_mut(&name) else {
                    return;
                };
                // Finished: dropping the query only releases its route.
                seen.resolving = None;
                match instance.get() {
                    Some(data) if status == ERROR_SUCCESS => {
                        seen.resolved = Some(found_from(data, &name));
                        seen.failures = 0;
                    }
                    _ => seen.failures += 1,
                }
            }
            Msg::Registered { .. } => {}
        }
    }

    /// Remember addresses from browse answers until their TTL runs out.
    fn learn_hosts(&mut self, hosts: Vec<(String, IpAddr, u32)>) {
        let now = Instant::now();
        for (host, ip, ttl) in hosts {
            // Link-local IPv6 needs its interface, which browse answers
            // don't carry.
            if matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()) {
                continue;
            }
            if ttl == 0 {
                if let Some(ips) = self.hosts.get_mut(&host) {
                    ips.remove(&ip);
                }
                continue;
            }
            if !self.hosts.contains_key(&host) && self.hosts.len() >= MAX_ENGINES {
                self.prune_hosts(now);
                if self.hosts.len() >= MAX_ENGINES && !self.evict_host() {
                    continue;
                }
            }
            let ips = self.hosts.entry(host).or_default();
            if ips.contains_key(&ip) || ips.len() < ADDRS_PER_HOST {
                let ttl = Duration::from_secs(u64::from(ttl.min(MAX_ADDRESS_TTL)));
                ips.insert(ip, now + ttl);
            }
        }
    }

    fn prune_hosts(&mut self, now: Instant) {
        for ips in self.hosts.values_mut() {
            ips.retain(|_, expires| *expires > now);
        }
        self.hosts.retain(|_, ips| !ips.is_empty());
    }

    /// Make room: drop the host no known engine uses whose addresses
    /// expire soonest. Whether one was dropped.
    fn evict_host(&mut self) -> bool {
        let used: Vec<String> = self
            .instances
            .values()
            .filter_map(|seen| seen.resolved.as_ref().map(|f| host_key(&f.host)))
            .collect();
        let victim = self
            .hosts
            .iter()
            .filter(|(host, _)| !used.contains(host))
            .min_by_key(|(_, ips)| ips.values().max().copied())
            .map(|(host, _)| host.clone());
        victim.is_some_and(|host| self.hosts.remove(&host).is_some())
    }

    /// Full: make room for an instance just heard on the network by
    /// dropping a never-confirmed one that hasn't been heard lately (the
    /// cache's stale instances), the lowest TTL first. Answers only from
    /// the cache never displace anything. Whether room was made.
    fn make_room(&mut self, heard: bool) -> bool {
        if !heard {
            return false;
        }
        let victim = self
            .instances
            .iter()
            .filter(|(_, seen)| seen.resolved.is_none() && !seen.heard_lately())
            .min_by_key(|(_, seen)| seen.ttl)
            .map(|(name, _)| name.clone());
        victim.is_some_and(|name| {
            self.forget(&name);
            true
        })
    }

    fn forget(&mut self, name: &str) {
        if let Some(seen) = self.instances.remove(name)
            && let Some((query, _)) = seen.resolving
        {
            self.resolving.remove(&query.token);
        }
    }

    /// Time out slow resolves, start due ones, forget the long silent and
    /// expired addresses.
    fn check(&mut self) {
        let now = Instant::now();
        self.prune_hosts(now);
        for seen in self.instances.values_mut() {
            if seen
                .resolving
                .as_ref()
                .is_some_and(|(_, at)| now - *at >= RESOLVE_TIMEOUT)
                && let Some((query, _)) = seen.resolving.take()
            {
                self.resolving.remove(&query.token);
                seen.failures += 1;
            }
        }
        let silent: Vec<String> = self
            .instances
            .iter()
            .filter(|(_, seen)| seen.failures >= FORGET_AFTER)
            .map(|(name, _)| name.clone())
            .collect();
        self.forgotten
            .retain(|_, at| now.duration_since(*at) < IGNORE_FORGOTTEN);
        for name in silent {
            self.forget(&name);
            if self.forgotten.len() < MAX_ENGINES * 8 {
                self.forgotten.insert(name, now);
            }
        }
        // Resolves run a few at a time (Windows completed none of 64
        // concurrent ones for stale instances): known engines first, then
        // those heard on the network lately, then the longest TTL.
        let mut due: Vec<((bool, bool, u32), String)> = self
            .instances
            .iter()
            .filter(|(_, seen)| seen.due(now))
            .map(|(name, seen)| {
                let rank = (seen.resolved.is_some(), seen.heard_lately(), seen.ttl);
                (rank, name.clone())
            })
            .collect();
        due.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        for ((known, heard, _), name) in due {
            if self.resolving.len() >= MAX_RESOLVING && !((known || heard) && self.preempt()) {
                break;
            }
            let Some(seen) = self.instances.get_mut(&name) else {
                continue;
            };
            seen.checked = Some(now);
            match start_resolve(&self.send, &seen.full) {
                Ok(query) => {
                    self.resolving.insert(query.token, name);
                    seen.resolving = Some((query, now));
                }
                // Refused at once: a failure like any other, with backoff.
                Err(Refused::System) => seen.failures += 1,
                // Degraded: say so, and start nothing more this loop (not
                // the engine's fault, so not a failure).
                Err(Refused::Budget(reason)) => {
                    self.error(reason);
                    break;
                }
            }
        }
    }

    /// Free a resolve slot for a known or just-heard engine by stopping a
    /// resolve of a cache-only instance (it isn't charged a failure and
    /// is tried again later). Whether a slot was freed.
    fn preempt(&mut self) -> bool {
        let victim = self
            .instances
            .iter()
            .find(|(_, seen)| {
                seen.resolving.is_some() && seen.resolved.is_none() && !seen.heard_lately()
            })
            .map(|(name, _)| name.clone());
        let Some(seen) = victim.and_then(|name| self.instances.get_mut(&name)) else {
            return false;
        };
        if let Some((query, _)) = seen.resolving.take() {
            self.resolving.remove(&query.token);
        }
        seen.checked = None;
        true
    }

    fn report(&mut self) {
        let mut now: BTreeMap<String, Found> = BTreeMap::new();
        for (name, seen) in &self.instances {
            if seen.failures >= LOST_AFTER {
                continue;
            }
            if let Some(found) = &seen.resolved {
                let mut found = found.clone();
                if let Some(ips) = self.hosts.get(&host_key(&found.host)) {
                    let port = found.port;
                    found
                        .addrs
                        .extend(ips.keys().map(|ip| SocketAddr::new(*ip, port)));
                }
                crate::sort_addrs(&mut found.addrs);
                found.addrs.truncate(ADDRS_PER_HOST);
                if !found.addrs.is_empty() {
                    now.insert(name.clone(), found);
                }
            }
        }
        let gone: Vec<String> = self
            .reported
            .keys()
            .filter(|name| !now.contains_key(*name))
            .cloned()
            .collect();
        for name in gone {
            self.reported.remove(&name);
            (self.on_event)(Event::Lost { name });
        }
        for (name, found) in now {
            if self.reported.get(&name) != Some(&found) {
                self.reported.insert(name, found.clone());
                (self.on_event)(Event::Found(found));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_come_from_full_service_names() {
        assert_eq!(
            instance_of("exon-desk._isocast._tcp.local").as_deref(),
            Some("exon-desk")
        );
        assert_eq!(
            instance_of("A.b._ISOCAST._TCP.LOCAL.").as_deref(),
            Some("A.b")
        );
        assert_eq!(instance_of("._isocast._tcp.local"), None);
        assert_eq!(instance_of("x._http._tcp.local"), None);
        assert_eq!(instance_of("local"), None);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(20), Duration::from_secs(30));
    }
}
