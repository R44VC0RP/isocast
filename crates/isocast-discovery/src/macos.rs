//! Bonjour through `dns_sd.h` (libSystem). Each operation is a connection
//! to mDNSResponder; replies arrive on its socket and are delivered by
//! `DNSServiceProcessResult` on our own thread, so callbacks only ever run
//! inside our loops and their contexts outlive them. How a watch bounds
//! that call is explained at the top of its section.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::{Event, Found, MAX_ENGINES, OnEvent, SERVICE_TYPE, Txt};

pub const SUPPORTED: bool = true;

type DnsServiceRef = *mut c_void;
type Flags = u32;
type ErrorType = i32;

const FLAGS_MORE_COMING: Flags = 0x1;
const FLAGS_ADD: Flags = 0x2;
const PROTOCOL_IPV4: u32 = 0x01;
const PROTOCOL_IPV6: u32 = 0x02;
const ERR_NONE: ErrorType = 0;
const ERR_SERVICE_NOT_RUNNING: ErrorType = -65563;
const ERR_POLICY_DENIED: ErrorType = -65570;

type RegisterReply = unsafe extern "C" fn(
    DnsServiceRef,
    Flags,
    ErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type BrowseReply = unsafe extern "C" fn(
    DnsServiceRef,
    Flags,
    u32,
    ErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveReply = unsafe extern "C" fn(
    DnsServiceRef,
    Flags,
    u32,
    ErrorType,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);
type AddrInfoReply = unsafe extern "C" fn(
    DnsServiceRef,
    Flags,
    u32,
    ErrorType,
    *const c_char,
    *const libc::sockaddr,
    u32,
    *mut c_void,
);

unsafe extern "C" {
    fn DNSServiceRegister(
        sd: *mut DnsServiceRef,
        flags: Flags,
        interface: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt: *const c_void,
        callback: Option<RegisterReply>,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceBrowse(
        sd: *mut DnsServiceRef,
        flags: Flags,
        interface: u32,
        regtype: *const c_char,
        domain: *const c_char,
        callback: Option<BrowseReply>,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceResolve(
        sd: *mut DnsServiceRef,
        flags: Flags,
        interface: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        callback: Option<ResolveReply>,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceGetAddrInfo(
        sd: *mut DnsServiceRef,
        flags: Flags,
        interface: u32,
        protocol: u32,
        hostname: *const c_char,
        callback: Option<AddrInfoReply>,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceRefSockFD(sd: DnsServiceRef) -> c_int;
    fn DNSServiceProcessResult(sd: DnsServiceRef) -> ErrorType;
    fn DNSServiceRefDeallocate(sd: DnsServiceRef);
}

fn describe(code: ErrorType) -> String {
    match code {
        ERR_POLICY_DENIED => "Local Network access is off for this app: turn it on in System \
                              Settings → Privacy & Security → Local Network"
            .to_owned(),
        ERR_SERVICE_NOT_RUNNING => "the Bonjour service (mDNSResponder) isn't running".to_owned(),
        code => format!("Bonjour error {code}"),
    }
}

/// An open DNS-SD operation; deallocating it ends the operation (for a
/// registration, that sends the goodbye).
struct Sd(DnsServiceRef);

// SAFETY: a DNSServiceRef may be used from any one thread at a time; each
// `Sd` is owned and used by one thread after it is created.
unsafe impl Send for Sd {}

impl Sd {
    fn fd(&self) -> c_int {
        // SAFETY: a live ref from a successful DNSService* call.
        unsafe { DNSServiceRefSockFD(self.0) }
    }

    fn process(&self) -> ErrorType {
        // SAFETY: as above; callbacks run on this thread, inside this call.
        unsafe { DNSServiceProcessResult(self.0) }
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        // SAFETY: deallocated exactly once, by its owner.
        unsafe { DNSServiceRefDeallocate(self.0) }
    }
}

/// Wait up to `timeout` for any of `fds` to be readable; which ones are.
fn wait_readable(fds: &[c_int], timeout: Duration) -> Vec<bool> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = c_int::try_from(timeout.as_millis()).unwrap_or(c_int::MAX);
    // SAFETY: `polls` is a valid array of `polls.len()` entries.
    let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
    if ready <= 0 {
        return vec![false; fds.len()];
    }
    polls
        .iter()
        .map(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        .collect()
}

fn text(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: Bonjour passes NUL-terminated strings valid for the callback.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Parse a TXT record's wire format (length-prefixed `key=value` strings).
/// A truncated last item ends parsing; it isn't read.
fn txt_from_wire(bytes: &[u8]) -> Txt {
    let mut txt = Txt::default();
    let mut rest = bytes;
    while let Some((&len, tail)) = rest.split_first() {
        let Some((item, tail)) = tail.split_at_checked(usize::from(len)) else {
            break;
        };
        rest = tail;
        let item = String::from_utf8_lossy(item);
        if let Some((key, value)) = item.split_once('=') {
            txt.set(key, value);
        }
    }
    txt
}

/// The wire format, checked: each `key=value` must fit in 255 bytes.
fn txt_to_wire(txt: &Txt) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (key, value) in txt.pairs() {
        let item = format!("{key}={value}");
        out.push(u8::try_from(item.len()).map_err(|_| anyhow!("txt.{key}: too long"))?);
        out.extend_from_slice(item.as_bytes());
    }
    Ok(out)
}

// ---------------------------------------------------------------- register

pub struct Registration {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    name: Arc<Mutex<String>>,
}

impl Registration {
    pub fn name(&self) -> String {
        self.name.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Advert {
    name: CString,
    regtype: CString,
    port: u16,
    txt: Vec<u8>,
}

/// What a registration callback reports: the name in use, or an error.
type Registered = RefCell<Option<Result<String, ErrorType>>>;

unsafe extern "C" fn on_registered(
    _sd: DnsServiceRef,
    _flags: Flags,
    error: ErrorType,
    name: *const c_char,
    _regtype: *const c_char,
    _domain: *const c_char,
    context: *mut c_void,
) {
    // SAFETY: the context is the `Registered` the caller keeps alive
    // while it processes this ref's results. The name is only defined
    // without an error.
    let slot = unsafe { &*context.cast::<Registered>() };
    *slot.borrow_mut() = Some(if error == ERR_NONE {
        Ok(text(name))
    } else {
        Err(error)
    });
}

/// Start advertising; the ref and the context its callback writes to.
fn start(advert: &Advert) -> Result<(Sd, Box<Registered>)> {
    let slot: Box<Registered> = Box::new(RefCell::new(None));
    let mut sd: DnsServiceRef = std::ptr::null_mut();
    // SAFETY: valid C strings and TXT bytes (copied by the call); the
    // context outlives every ProcessResult on this ref (both are dropped
    // together, the ref first: see `run_registration`).
    let error = unsafe {
        DNSServiceRegister(
            &mut sd,
            0,
            0,
            advert.name.as_ptr(),
            advert.regtype.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            advert.port.to_be(),
            advert.txt.len() as u16,
            advert.txt.as_ptr().cast(),
            Some(on_registered),
            (&raw const *slot).cast_mut().cast(),
        )
    };
    if error != ERR_NONE {
        bail!("could not advertise: {}", describe(error));
    }
    Ok((Sd(sd), slot))
}

pub fn register(name: &str, port: u16, txt: &Txt) -> Result<Registration> {
    let advert = Advert {
        name: CString::new(name).context("name: contains a NUL character")?,
        regtype: CString::new(SERVICE_TYPE).expect("no NUL"),
        port,
        txt: txt_to_wire(txt)?,
    };
    let (sd, slot) = start(&advert)?;
    // mDNSResponder answers once it has probed the name (about a second).
    let deadline = Instant::now() + Duration::from_secs(5);
    let actual = loop {
        if let Some(result) = slot.borrow_mut().take() {
            break result
                .map_err(|code| anyhow!("could not advertise {name}: {}", describe(code)))?;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            // Dropping the ref here withdraws whatever was started.
            bail!("could not advertise {name}: Bonjour didn't confirm within 5 s");
        }
        if wait_readable(&[sd.fd()], left)[0] {
            let error = sd.process();
            if error != ERR_NONE {
                bail!("could not advertise {name}: {}", describe(error));
            }
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let current = Arc::new(Mutex::new(actual));
    let thread = std::thread::Builder::new()
        .name("isocast-advertise".into())
        .spawn({
            let stop = stop.clone();
            let current = current.clone();
            move || run_registration(advert, sd, slot, &current, &stop)
        })
        .context("could not start the advertising thread")?;
    Ok(Registration {
        stop,
        thread: Some(thread),
        name: current,
    })
}

/// Keep the registration's socket read (renames after a later conflict
/// arrive here and update `current`), and advertise again if
/// mDNSResponder restarts.
fn run_registration(
    advert: Advert,
    sd: Sd,
    slot: Box<Registered>,
    current: &Mutex<String>,
    stop: &AtomicBool,
) {
    let mut active = Some((sd, slot));
    let mut retry_at = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let Some((sd, slot)) = &active else {
            if Instant::now() >= retry_at {
                match start(&advert) {
                    Ok(started) => active = Some(started),
                    Err(error) => {
                        isocast_diagnostics::diagnostic!("warn"; "discovery: {error:#}; retrying");
                        retry_at = Instant::now() + Duration::from_secs(5);
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(200));
            }
            continue;
        };
        if !wait_readable(&[sd.fd()], Duration::from_millis(200))[0] {
            continue;
        }
        let error = sd.process();
        let report = slot.borrow_mut().take();
        match (error, report) {
            (ERR_NONE, Some(Ok(name))) => {
                let mut current = current.lock().unwrap_or_else(|e| e.into_inner());
                if *current != name {
                    isocast_diagnostics::diagnostic!("info"; "discovery: now advertising as {name:?}");
                    *current = name;
                }
            }
            (ERR_NONE, None) => {}
            (error, report) => {
                let code = report.and_then(|r| r.err()).unwrap_or(error);
                isocast_diagnostics::diagnostic!(
                    "warn";
                    "discovery: advertisement stopped ({}); advertising again",
                    describe(code)
                );
                active = None;
                retry_at = Instant::now() + Duration::from_secs(1);
            }
        }
    }
    // The ref is deallocated before its context (tuple order), which
    // sends the goodbye.
    drop(active);
}

// ---------------------------------------------------------------- watch
//
// Bounding the work: `DNSServiceProcessResult` doesn't handle one reply
// and return. It loops while the socket has more bytes
// (`do { … } while (morebytes)` in mDNSResponder's dnssd_clientstub.c),
// so under a flood one call can run indefinitely, and a dispatch queue
// runs the same loop in its event handler. The one supported way out is
// `DNSServiceRefDeallocate` from inside a callback: it clears the loop's
// `morebytes` and the call returns without touching the ref again (the
// "CAUTION" comment beside that loop). So every callback first checks
// the stop flag, the loop's time budget and the reply queue, and past any
// of them ends its own operation; the browser restarts it (or exits).

/// Replies kept between applying them; a full queue ends the operation
/// delivering more (it is restarted), so nothing is silently dropped.
const QUEUE_LIMIT: usize = 512;
/// Time spent reading sockets per loop, and per operation within it.
const ITERATION_BUDGET: Duration = Duration::from_millis(50);
const OPERATION_SLICE: Duration = Duration::from_millis(10);
/// Failed resolves (or address lookups) in a row before an engine is left
/// alone, until it is announced again.
const MAX_FAILURES: u32 = 5;
const ADDRS_PER_ENGINE: usize = 16;

fn backoff(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.min(5)).min(60))
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
    let stop = Arc::new(AtomicBool::new(false));
    // Fail here, not on the thread, if browsing can't start at all.
    let (ready_send, ready) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("isocast-discovery".into())
        .spawn({
            let stop = stop.clone();
            move || Browser::new(on_event, stop).run(ready_send)
        })
        .context("could not start the discovery thread")?;
    match ready.recv() {
        Ok(Ok(())) => Ok(Watch {
            stop,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            bail!("the discovery thread stopped")
        }
    }
}

/// One service instance: a name seen on one interface.
type Instance = (String, u32);

/// Which operation a context belongs to; `generation` tells replies of a
/// replaced operation from its successor's.
#[derive(Clone)]
enum Tag {
    Browse { generation: u64 },
    Resolve { instance: Instance, generation: u64 },
    Address { instance: Instance, generation: u64 },
}

enum Reply {
    BrowseFailed {
        generation: u64,
        error: ErrorType,
    },
    Browse {
        generation: u64,
        add: bool,
        /// Bonjour has more replies queued (the initial batch isn't done).
        more: bool,
        instance: Instance,
        regtype: String,
        domain: String,
    },
    ResolveFailed {
        instance: Instance,
        generation: u64,
        error: ErrorType,
    },
    Resolved {
        instance: Instance,
        generation: u64,
        host: String,
        port: u16,
        txt: Txt,
    },
    AddressFailed {
        instance: Instance,
        generation: u64,
        error: ErrorType,
    },
    Address {
        instance: Instance,
        generation: u64,
        add: bool,
        addr: SocketAddr,
    },
}

/// What every callback can see: where replies go, and when to stop.
struct Shared {
    queue: RefCell<Vec<Reply>>,
    stop: Arc<AtomicBool>,
    /// The end of the current loop's reading budget.
    deadline: Cell<Instant>,
}

/// A callback's context: its operation, and the ref, so the callback can
/// end it. Boxed in its `Op`; only used on the browser's thread, inside
/// `DNSServiceProcessResult`.
struct Ctx {
    tag: Tag,
    shared: *const Shared,
    sd: Cell<DnsServiceRef>,
    /// The callback deallocated the ref.
    ended: Cell<bool>,
}

/// An operation owned by the browser; dropping it deallocates the ref
/// unless its callback already did.
struct Op {
    ctx: Box<Ctx>,
}

enum Processed {
    Done(ErrorType),
    /// A callback ended the operation (stop, budget or a full queue).
    Ended,
}

impl Op {
    fn fd(&self) -> c_int {
        // SAFETY: a live ref (not ended: ended ops are dropped at once).
        unsafe { DNSServiceRefSockFD(self.ctx.sd.get()) }
    }

    fn process(&self) -> Processed {
        // SAFETY: a live ref; callbacks run on this thread, inside this
        // call, and may deallocate it (then the call returns at once).
        let error = unsafe { DNSServiceProcessResult(self.ctx.sd.get()) };
        if self.ctx.ended.get() {
            Processed::Ended
        } else {
            Processed::Done(error)
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if !self.ctx.ended.get() {
            // SAFETY: deallocated exactly once: here or by its callback.
            unsafe { DNSServiceRefDeallocate(self.ctx.sd.get()) }
        }
    }
}

/// The callback's context, or `None` when it must stop: then it has
/// ended its own operation (see the top of this section).
///
/// SAFETY: `context` is a live `Ctx` whose `Shared` outlives it; called
/// only from callbacks, inside `DNSServiceProcessResult` on the browser's
/// thread.
unsafe fn admit<'a>(context: *mut c_void) -> Option<(&'a Ctx, &'a Shared)> {
    let ctx = unsafe { &*context.cast::<Ctx>() };
    let shared = unsafe { &*ctx.shared };
    let over = shared.stop.load(Ordering::Acquire)
        || Instant::now() >= shared.deadline.get()
        || shared.queue.borrow().len() >= QUEUE_LIMIT;
    if over {
        if !ctx.ended.get() {
            ctx.ended.set(true);
            // SAFETY: the supported way to leave DNSServiceProcessResult's
            // loop; the ref isn't touched again (Op skips its drop).
            unsafe { DNSServiceRefDeallocate(ctx.sd.get()) };
        }
        return None;
    }
    Some((ctx, shared))
}

// The callbacks read their result parameters only without an error: on an
// error dns_sd.h leaves them undefined.

unsafe extern "C" fn on_browse(
    _sd: DnsServiceRef,
    flags: Flags,
    interface: u32,
    error: ErrorType,
    name: *const c_char,
    regtype: *const c_char,
    domain: *const c_char,
    context: *mut c_void,
) {
    // SAFETY: see `admit`.
    let Some((ctx, shared)) = (unsafe { admit(context) }) else {
        return;
    };
    let Tag::Browse { generation } = ctx.tag else {
        return;
    };
    let reply = if error != ERR_NONE {
        Reply::BrowseFailed { generation, error }
    } else {
        Reply::Browse {
            generation,
            add: flags & FLAGS_ADD != 0,
            more: flags & FLAGS_MORE_COMING != 0,
            instance: (crate::clean(&text(name), 255), interface),
            regtype: text(regtype),
            domain: text(domain),
        }
    };
    shared.queue.borrow_mut().push(reply);
}

unsafe extern "C" fn on_resolve(
    _sd: DnsServiceRef,
    _flags: Flags,
    _interface: u32,
    error: ErrorType,
    _fullname: *const c_char,
    host: *const c_char,
    port: u16,
    txt_len: u16,
    txt: *const u8,
    context: *mut c_void,
) {
    // SAFETY: see `admit`.
    let Some((ctx, shared)) = (unsafe { admit(context) }) else {
        return;
    };
    let Tag::Resolve {
        instance,
        generation,
    } = ctx.tag.clone()
    else {
        return;
    };
    let reply = if error != ERR_NONE {
        Reply::ResolveFailed {
            instance,
            generation,
            error,
        }
    } else {
        let txt = if txt.is_null() {
            Txt::default()
        } else {
            // SAFETY: Bonjour passes `txt_len` bytes valid for the callback.
            txt_from_wire(unsafe { std::slice::from_raw_parts(txt, usize::from(txt_len)) })
        };
        Reply::Resolved {
            instance,
            generation,
            host: text(host),
            port: u16::from_be(port),
            txt,
        }
    };
    shared.queue.borrow_mut().push(reply);
}

unsafe extern "C" fn on_address(
    _sd: DnsServiceRef,
    flags: Flags,
    interface: u32,
    error: ErrorType,
    _host: *const c_char,
    address: *const libc::sockaddr,
    _ttl: u32,
    context: *mut c_void,
) {
    // SAFETY: see `admit`.
    let Some((ctx, shared)) = (unsafe { admit(context) }) else {
        return;
    };
    let Tag::Address {
        instance,
        generation,
    } = ctx.tag.clone()
    else {
        return;
    };
    if error != ERR_NONE {
        shared.queue.borrow_mut().push(Reply::AddressFailed {
            instance,
            generation,
            error,
        });
        return;
    }
    if address.is_null() {
        return;
    }
    // SAFETY: Bonjour passes a sockaddr of its family's size.
    if let Some(addr) = unsafe { socket_addr(address, interface) } {
        shared.queue.borrow_mut().push(Reply::Address {
            instance,
            generation,
            add: flags & FLAGS_ADD != 0,
            addr,
        });
    }
}

/// SAFETY: `address` points at a sockaddr_in or sockaddr_in6 per its
/// family.
unsafe fn socket_addr(address: *const libc::sockaddr, interface: u32) -> Option<SocketAddr> {
    match i32::from(unsafe { (*address).sa_family }) {
        libc::AF_INET => {
            let v4 = unsafe { &*address.cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr));
            Some(SocketAddr::V4(SocketAddrV4::new(ip, 0)))
        }
        libc::AF_INET6 => {
            let v6 = unsafe { &*address.cast::<libc::sockaddr_in6>() };
            let ip = Ipv6Addr::from(v6.sin6_addr.s6_addr);
            let scope = if ip.is_unicast_link_local() {
                if v6.sin6_scope_id != 0 {
                    v6.sin6_scope_id
                } else {
                    interface
                }
            } else {
                0
            };
            Some(SocketAddr::V6(SocketAddrV6::new(ip, 0, 0, scope)))
        }
        _ => None,
    }
}

#[derive(Default)]
struct Seen {
    regtype: String,
    domain: String,
    /// The browse generation that last announced it.
    browse_generation: u64,
    /// Kept running: it reports SRV and TXT changes too.
    resolve: Option<(Op, u64)>,
    resolve_failures: u32,
    resolve_at: Option<Instant>,
    addresses: Option<(Op, u64)>,
    address_failures: u32,
    address_at: Option<Instant>,
    /// The host the address query follows.
    host: Option<String>,
    resolved: Option<(String, u16, Txt)>,
    addrs: BTreeSet<SocketAddr>,
}

/// Which socket a poll entry is.
#[derive(Clone)]
enum Which {
    Browse,
    Resolve(Instance),
    Address(Instance),
}

struct Browser {
    on_event: OnEvent,
    shared: Box<Shared>,
    browse: Option<(Op, u64)>,
    /// The current browse's last reply said more were coming (its batch
    /// isn't complete). No reply at all counts as complete: zero results.
    more_coming: bool,
    browse_started: Instant,
    restart_at: Option<Instant>,
    generations: u64,
    instances: BTreeMap<Instance, Seen>,
    reported: BTreeMap<String, Found>,
    /// The last error reported, so it is said once.
    last_error: Option<String>,
}

impl Browser {
    fn new(on_event: OnEvent, stop: Arc<AtomicBool>) -> Browser {
        Browser {
            on_event,
            shared: Box::new(Shared {
                queue: RefCell::new(Vec::new()),
                stop,
                deadline: Cell::new(Instant::now()),
            }),
            browse: None,
            more_coming: false,
            browse_started: Instant::now(),
            restart_at: None,
            generations: 0,
            instances: BTreeMap::new(),
            reported: BTreeMap::new(),
            last_error: None,
        }
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    fn next_generation(&mut self) -> u64 {
        self.generations += 1;
        self.generations
    }

    fn op(&self, tag: Tag) -> Op {
        Op {
            ctx: Box::new(Ctx {
                tag,
                shared: &raw const *self.shared,
                sd: Cell::new(std::ptr::null_mut()),
                ended: Cell::new(false),
            }),
        }
    }

    /// Finish creating `op` from its `DNSService*` call's result.
    fn started(op: Op, sd: DnsServiceRef, error: ErrorType) -> Result<Op, ErrorType> {
        if error != ERR_NONE {
            // Never created: nothing to deallocate.
            op.ctx.ended.set(true);
            return Err(error);
        }
        op.ctx.sd.set(sd);
        Ok(op)
    }

    fn start_browse(&mut self) -> Result<(), ErrorType> {
        let generation = self.next_generation();
        let op = self.op(Tag::Browse { generation });
        let regtype = CString::new(SERVICE_TYPE).expect("no NUL");
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: valid strings; the context lives in the returned Op,
        // which outlives every ProcessResult on the ref.
        let error = unsafe {
            DNSServiceBrowse(
                &mut sd,
                0,
                0,
                regtype.as_ptr(),
                std::ptr::null(),
                Some(on_browse),
                (&raw const *op.ctx).cast_mut().cast(),
            )
        };
        let op = Self::started(op, sd, error)?;
        self.browse = Some((op, generation));
        self.more_coming = false;
        self.browse_started = Instant::now();
        Ok(())
    }

    fn start_resolve(&mut self, instance: &Instance) -> Result<(), ErrorType> {
        let Some(seen) = self.instances.get(instance) else {
            return Ok(());
        };
        let (Ok(name), Ok(regtype), Ok(domain)) = (
            CString::new(instance.0.as_str()),
            CString::new(seen.regtype.as_str()),
            CString::new(seen.domain.as_str()),
        ) else {
            return Err(-65540); // kDNSServiceErr_BadParam
        };
        let generation = self.next_generation();
        let op = self.op(Tag::Resolve {
            instance: instance.clone(),
            generation,
        });
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: as in `start_browse`.
        let error = unsafe {
            DNSServiceResolve(
                &mut sd,
                0,
                instance.1,
                name.as_ptr(),
                regtype.as_ptr(),
                domain.as_ptr(),
                Some(on_resolve),
                (&raw const *op.ctx).cast_mut().cast(),
            )
        };
        let op = Self::started(op, sd, error)?;
        if let Some(seen) = self.instances.get_mut(instance) {
            seen.resolve = Some((op, generation));
            seen.resolve_at = None;
        }
        Ok(())
    }

    fn start_addresses(&mut self, instance: &Instance) -> Result<(), ErrorType> {
        let Some(host) = self.instances.get(instance).and_then(|s| s.host.clone()) else {
            return Ok(());
        };
        let Ok(host) = CString::new(host) else {
            return Err(-65540);
        };
        let generation = self.next_generation();
        let op = self.op(Tag::Address {
            instance: instance.clone(),
            generation,
        });
        let mut sd: DnsServiceRef = std::ptr::null_mut();
        // SAFETY: as in `start_browse`.
        let error = unsafe {
            DNSServiceGetAddrInfo(
                &mut sd,
                0,
                instance.1,
                PROTOCOL_IPV4 | PROTOCOL_IPV6,
                host.as_ptr(),
                Some(on_address),
                (&raw const *op.ctx).cast_mut().cast(),
            )
        };
        let op = Self::started(op, sd, error)?;
        if let Some(seen) = self.instances.get_mut(instance) {
            seen.addresses = Some((op, generation));
            seen.addrs.clear();
            seen.address_at = None;
        }
        Ok(())
    }

    /// Say a browsing failure once (until browsing works again).
    fn error(&mut self, code: ErrorType) {
        let reason = describe(code);
        if self.last_error.as_ref() != Some(&reason) {
            self.last_error = Some(reason.clone());
            (self.on_event)(Event::Error(reason));
        }
    }

    /// The browse failed: drop it and try again later.
    fn browse_failed(&mut self, code: ErrorType) {
        self.browse = None;
        let delay = if code == ERR_SERVICE_NOT_RUNNING {
            // mDNSResponder restarted: its instances are gone with it.
            self.instances.clear();
            Duration::from_secs(1)
        } else {
            Duration::from_secs(10)
        };
        self.restart_at = Some(Instant::now() + delay);
        self.error(code);
    }

    /// A resolve failed: stop following the instance and retry with
    /// backoff, at most [`MAX_FAILURES`] times in a row.
    fn resolve_failed(&mut self, instance: &Instance, code: ErrorType) {
        if code == ERR_POLICY_DENIED {
            self.error(code);
        }
        let Some(seen) = self.instances.get_mut(instance) else {
            return;
        };
        seen.resolve = None;
        seen.addresses = None;
        seen.address_at = None;
        seen.host = None;
        seen.resolved = None;
        seen.addrs.clear();
        seen.resolve_failures += 1;
        seen.resolve_at = (seen.resolve_failures < MAX_FAILURES)
            .then(|| Instant::now() + backoff(seen.resolve_failures));
    }

    /// An address lookup failed: the resolve keeps running; the lookup is
    /// retried with its own backoff.
    fn address_failed(&mut self, instance: &Instance, code: ErrorType) {
        if code == ERR_POLICY_DENIED {
            self.error(code);
        }
        let Some(seen) = self.instances.get_mut(instance) else {
            return;
        };
        seen.addresses = None;
        seen.addrs.clear();
        seen.address_failures += 1;
        seen.address_at = (seen.address_failures < MAX_FAILURES)
            .then(|| Instant::now() + backoff(seen.address_failures));
    }

    fn run(mut self, ready: mpsc::Sender<Result<()>>) {
        if let Err(code) = self.start_browse() {
            let _ = ready.send(Err(anyhow!(
                "could not browse for engines: {}",
                describe(code)
            )));
            return;
        }
        let _ = ready.send(Ok(()));
        while !self.stopping() {
            if self.browse.is_none() && self.restart_at.is_some_and(|at| Instant::now() >= at) {
                self.restart_at = None;
                if let Err(code) = self.start_browse() {
                    self.browse_failed(code);
                }
            }
            self.retry();
            let (which, fds) = self.sockets();
            if fds.is_empty() {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            let readable = wait_readable(&fds, Duration::from_millis(200));
            let budget_end = Instant::now() + ITERATION_BUDGET;
            for (which, ready) in which.into_iter().zip(readable) {
                let now = Instant::now();
                if !ready {
                    continue;
                }
                if self.stopping() || now >= budget_end {
                    break;
                }
                // Each operation gets a slice, so a flooded browse can't
                // starve the resolves and lookups (served first).
                self.shared
                    .deadline
                    .set((now + OPERATION_SLICE).min(budget_end));
                self.process(which);
            }
            if self.stopping() {
                break;
            }
            self.apply();
            self.report();
        }
    }

    /// Every open operation's socket, the browse last.
    fn sockets(&self) -> (Vec<Which>, Vec<c_int>) {
        let mut which = Vec::new();
        let mut fds = Vec::new();
        for (instance, seen) in &self.instances {
            if let Some((op, _)) = &seen.resolve {
                which.push(Which::Resolve(instance.clone()));
                fds.push(op.fd());
            }
            if let Some((op, _)) = &seen.addresses {
                which.push(Which::Address(instance.clone()));
                fds.push(op.fd());
            }
        }
        if let Some((op, _)) = &self.browse {
            which.push(Which::Browse);
            fds.push(op.fd());
        }
        (which, fds)
    }

    /// Process the results waiting on one operation, within the budget.
    fn process(&mut self, which: Which) {
        let result = match &which {
            Which::Browse => self.browse.as_ref().map(|(op, _)| op.process()),
            Which::Resolve(instance) => self
                .instances
                .get(instance)
                .and_then(|seen| seen.resolve.as_ref())
                .map(|(op, _)| op.process()),
            Which::Address(instance) => self
                .instances
                .get(instance)
                .and_then(|seen| seen.addresses.as_ref())
                .map(|(op, _)| op.process()),
        };
        let now = Instant::now();
        match (which, result) {
            (_, None | Some(Processed::Done(ERR_NONE))) => {}
            // Ended by a callback: on stop the loop exits; otherwise the
            // operation is restarted right away (not a failure).
            (Which::Browse, Some(Processed::Ended)) => {
                self.browse = None;
                self.restart_at = Some(now);
            }
            (Which::Resolve(instance), Some(Processed::Ended)) => {
                if let Some(seen) = self.instances.get_mut(&instance) {
                    seen.resolve = None;
                    seen.resolve_at = Some(now);
                }
            }
            (Which::Address(instance), Some(Processed::Ended)) => {
                if let Some(seen) = self.instances.get_mut(&instance) {
                    seen.addresses = None;
                    seen.address_at = Some(now);
                }
            }
            (Which::Browse, Some(Processed::Done(code))) => self.browse_failed(code),
            (Which::Resolve(instance), Some(Processed::Done(code))) => {
                self.resolve_failed(&instance, code)
            }
            (Which::Address(instance), Some(Processed::Done(code))) => {
                self.address_failed(&instance, code)
            }
        }
    }

    /// Start the resolves and address lookups that are due.
    fn retry(&mut self) {
        let now = Instant::now();
        let mut resolves = Vec::new();
        let mut lookups = Vec::new();
        for (instance, seen) in &self.instances {
            if seen.resolve.is_none() && seen.resolve_at.is_some_and(|at| now >= at) {
                resolves.push(instance.clone());
            } else if seen.addresses.is_none()
                && seen.host.is_some()
                && seen.address_at.is_some_and(|at| now >= at)
            {
                lookups.push(instance.clone());
            }
        }
        for instance in resolves {
            if let Err(code) = self.start_resolve(&instance) {
                self.resolve_failed(&instance, code);
            }
        }
        for instance in lookups {
            if let Err(code) = self.start_addresses(&instance) {
                self.address_failed(&instance, code);
            }
        }
    }

    fn current_resolve(&self, instance: &Instance, generation: u64) -> bool {
        self.instances
            .get(instance)
            .and_then(|seen| seen.resolve.as_ref())
            .is_some_and(|(_, current)| *current == generation)
    }

    fn current_addresses(&self, instance: &Instance, generation: u64) -> bool {
        self.instances
            .get(instance)
            .and_then(|seen| seen.addresses.as_ref())
            .is_some_and(|(_, current)| *current == generation)
    }

    /// Apply the queued replies (those of replaced operations are
    /// ignored).
    fn apply(&mut self) {
        let replies = std::mem::take(&mut *self.shared.queue.borrow_mut());
        let browsing = self.browse.as_ref().map(|(_, generation)| *generation);
        for reply in replies {
            match reply {
                Reply::BrowseFailed { generation, error } => {
                    if Some(generation) == browsing {
                        self.browse_failed(error);
                    }
                }
                Reply::Browse {
                    generation,
                    add,
                    more,
                    instance,
                    regtype,
                    domain,
                } => {
                    if Some(generation) != browsing {
                        continue;
                    }
                    self.last_error = None;
                    self.more_coming = more;
                    if !add {
                        self.instances.remove(&instance);
                        continue;
                    }
                    if let Some(seen) = self.instances.get_mut(&instance) {
                        seen.browse_generation = generation;
                        // Announced again after its retries ran out: start over.
                        if seen.resolve.is_none() && seen.resolve_at.is_none() {
                            seen.resolve_failures = 0;
                            seen.address_failures = 0;
                            seen.resolve_at = Some(Instant::now());
                        } else if seen.host.is_some()
                            && seen.addresses.is_none()
                            && seen.address_at.is_none()
                        {
                            // Its address lookups ran out: look again.
                            seen.address_failures = 0;
                            seen.address_at = Some(Instant::now());
                        }
                        continue;
                    }
                    if self.instances.len() >= MAX_ENGINES {
                        continue;
                    }
                    let seen = Seen {
                        regtype,
                        domain,
                        browse_generation: generation,
                        resolve_at: Some(Instant::now()),
                        ..Seen::default()
                    };
                    // Resolved by `retry` on the next loop.
                    self.instances.insert(instance, seen);
                }
                Reply::ResolveFailed {
                    instance,
                    generation,
                    error,
                } => {
                    if self.current_resolve(&instance, generation) {
                        self.resolve_failed(&instance, error);
                    }
                }
                Reply::AddressFailed {
                    instance,
                    generation,
                    error,
                } => {
                    if self.current_addresses(&instance, generation) {
                        self.address_failed(&instance, error);
                    }
                }
                Reply::Resolved {
                    instance,
                    generation,
                    host,
                    port,
                    txt,
                } => {
                    if !self.current_resolve(&instance, generation) {
                        continue;
                    }
                    let host = crate::clean_host(&host);
                    let Some(seen) = self.instances.get_mut(&instance) else {
                        continue;
                    };
                    seen.resolve_failures = 0;
                    seen.resolved = Some((host.clone(), port, txt));
                    // A new host (or the first): follow its addresses
                    // with a new lookup (older replies are ignored).
                    if seen.host.as_deref() != Some(&host) {
                        seen.host = Some(host);
                        seen.addresses = None;
                        seen.addrs.clear();
                        seen.address_failures = 0;
                        if let Err(code) = self.start_addresses(&instance) {
                            self.address_failed(&instance, code);
                        }
                    }
                }
                Reply::Address {
                    instance,
                    generation,
                    add,
                    addr,
                } => {
                    if !self.current_addresses(&instance, generation) {
                        continue;
                    }
                    let Some(seen) = self.instances.get_mut(&instance) else {
                        continue;
                    };
                    seen.address_failures = 0;
                    if add {
                        if seen.addrs.len() < ADDRS_PER_ENGINE {
                            seen.addrs.insert(addr);
                        }
                    } else {
                        seen.addrs.remove(&addr);
                    }
                }
            }
        }
        // A restarted browse re-announces every instance it still sees;
        // once its initial batch is in, the rest went away meanwhile.
        // Settled: 2 s since the browse started (Bonjour may pause between
        // parts of a large batch) and its batch complete, judged now rather
        // than on a later reply, so a quiet network settles too.
        let settled = !self.more_coming && self.browse_started.elapsed() >= Duration::from_secs(2);
        if let (Some(generation), true) = (browsing, settled) {
            self.instances
                .retain(|_, seen| seen.browse_generation == generation);
        }
    }

    /// Tell the caller what changed, per name (one engine may be seen on
    /// several interfaces).
    fn report(&mut self) {
        let mut now: BTreeMap<String, Found> = BTreeMap::new();
        for ((name, _), seen) in &self.instances {
            let Some((host, port, txt)) = &seen.resolved else {
                continue;
            };
            let entry = now.entry(name.clone()).or_insert_with(|| Found {
                name: name.clone(),
                host: host.clone(),
                port: *port,
                addrs: Vec::new(),
                txt: txt.clone(),
            });
            entry.addrs.extend(seen.addrs.iter().map(|addr| {
                let mut addr = *addr;
                addr.set_port(*port);
                addr
            }));
        }
        now.retain(|_, found| {
            crate::sort_addrs(&mut found.addrs);
            found.addrs.truncate(ADDRS_PER_ENGINE);
            !found.addrs.is_empty()
        });
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
    fn txt_round_trip() {
        let txt = Txt {
            id: "abc".into(),
            proto: "1".into(),
            name: "Ryan's Mac".into(),
        };
        assert_eq!(txt_from_wire(&txt_to_wire(&txt).unwrap()), txt);
    }

    #[test]
    fn txt_ignores_unknown_keys_and_items_without_values() {
        let wire = b"\x06id=abc\x05other\x07foo=bar\x07proto=2";
        let txt = txt_from_wire(wire);
        assert_eq!(txt.id, "abc");
        assert_eq!(txt.proto, "2");
        assert_eq!(txt.name, "");
    }

    #[test]
    fn txt_stops_at_a_truncated_item() {
        // The second item claims 40 bytes but only 9 follow.
        let txt = txt_from_wire(b"\x06id=abc\x28name=evil");
        assert_eq!(txt.id, "abc");
        assert_eq!(txt.name, "");
        assert_eq!(txt_from_wire(b""), Txt::default());
        assert_eq!(txt_from_wire(b"\xff"), Txt::default());
    }

    #[test]
    fn txt_values_are_cleaned_and_capped() {
        let mut wire = vec![0u8];
        let item = format!("name=a\u{7}b{}", "x".repeat(240));
        wire[0] = item.len() as u8;
        wire.extend_from_slice(item.as_bytes());
        let txt = txt_from_wire(&wire);
        assert!(txt.name.starts_with("abx"));
        assert_eq!(txt.name.len(), crate::MAX_TXT_VALUE_BYTES);
    }

    #[test]
    fn oversized_txt_is_refused() {
        let txt = Txt {
            name: "x".repeat(300),
            ..Txt::default()
        };
        assert!(txt_to_wire(&txt).is_err());
    }
}
