//! Finding Isocast engines on the local network (DNS-SD, `_isocast._tcp`).
//!
//! Everything goes through the operating system's own responder: Bonjour
//! (`dns_sd.h` in libSystem) on macOS and the `DnsService*` API of
//! Windows 10 1809+. Isocast never opens UDP 5353 or parses a multicast
//! packet itself, so nothing on the network reaches a DNS parser in our
//! process (docs/remote-engines.md, decision 11). Other systems (Linux:
//! Avahi, later) answer [`supported`] `false` and fail every call.
//!
//! What is found is untrusted: anyone on the network can advertise any
//! name, id or address. It only says where to try connecting; who an
//! engine is comes from its pinned key.
//!
//! - [`register`] advertises an engine until the [`Registration`] drops;
//! - [`browse`] lists what answers within a timeout;
//! - [`watch`] follows engines appearing and going away, for the app.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as sys;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows)))]
use unsupported as sys;

/// The DNS-SD service type every Isocast engine advertises.
pub const SERVICE_TYPE: &str = "_isocast._tcp";

/// The longest instance name DNS-SD allows, in UTF-8 bytes.
pub const MAX_NAME_BYTES: usize = 63;

/// The longest TXT value we send or keep, in bytes.
pub const MAX_TXT_VALUE_BYTES: usize = 200;

/// The most engines one browse or watch keeps track of: anything past
/// this on the network is ignored, so a flood of advertisements can't
/// grow memory or open queries without bound.
pub const MAX_ENGINES: usize = 64;

/// An engine's TXT record: only these three keys, never the version or
/// OS (those come from `hello`, after authentication). On what was found,
/// a missing key is an empty string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Txt {
    /// The engine's id (its key's hash).
    pub id: String,
    /// The protocol it speaks, e.g. `1`.
    pub proto: String,
    /// The name it shows.
    pub name: String,
}

impl Txt {
    pub(crate) fn pairs(&self) -> [(&'static str, &str); 3] {
        [
            ("id", &self.id),
            ("proto", &self.proto),
            ("name", &self.name),
        ]
    }

    /// Keep a received key's value if it is one of ours (bounded), and
    /// drop anything else.
    pub(crate) fn set(&mut self, key: &str, value: &str) {
        let slot = match key.to_ascii_lowercase().as_str() {
            "id" => &mut self.id,
            "proto" => &mut self.proto,
            "name" => &mut self.name,
            _ => return,
        };
        *slot = clean(value, MAX_TXT_VALUE_BYTES);
    }

    fn validate(&self) -> Result<()> {
        for (key, value) in self.pairs() {
            if value.len() > MAX_TXT_VALUE_BYTES {
                bail!(
                    "txt.{key}: {} bytes; at most {MAX_TXT_VALUE_BYTES}",
                    value.len()
                );
            }
            if value.chars().any(char::is_control) {
                bail!("txt.{key}: control characters aren't allowed");
            }
        }
        Ok(())
    }
}

/// An engine that answered on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Its DNS-SD instance name (what it advertised; untrusted).
    pub name: String,
    /// The host its service record points at, like `exon-desk.local`.
    pub host: String,
    pub port: u16,
    /// Where to connect: IPv4 first, then IPv6 (link-local ones with
    /// their interface as the scope), each with `port`.
    pub addrs: Vec<SocketAddr>,
    pub txt: Txt,
}

/// A change seen by [`watch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An engine appeared, or what it advertises changed.
    Found(Found),
    /// An engine stopped advertising (or stopped answering).
    Lost { name: String },
    /// Browsing failed, with why (e.g. macOS Local Network access is off).
    /// The watch keeps retrying; engines it can't see aren't reported.
    Error(String),
}

/// What [`watch`] calls, on its own thread, for every change. It must not
/// block: the watch can't stop (or see the network) while it runs.
pub type OnEvent = Box<dyn FnMut(Event) + Send + 'static>;

/// An advertisement. Dropping it stops advertising (the OS sends the
/// goodbye), and waits briefly for that to finish.
pub struct Registration {
    inner: sys::Registration,
}

impl Registration {
    /// The name advertised now: the requested one, cleaned, or renamed by
    /// the OS after a conflict (macOS appends " (2)", Windows "(1)"),
    /// including a conflict found after registering (macOS).
    pub fn name(&self) -> String {
        self.inner.name()
    }
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// A running [`watch`]. Dropping it stops browsing, within about 200 ms
/// however busy the network is, provided the [`OnEvent`] callback doesn't
/// block and the OS's DNS-SD service answers its calls promptly (a hung
/// mDNSResponder or DNS Client can hold any call); no event is delivered
/// after the drop returns.
pub struct Watch {
    _inner: sys::Watch,
}

/// Whether this system has a DNS-SD backend.
pub fn supported() -> bool {
    sys::SUPPORTED
}

/// Advertise an engine listening on TCP `port` as `name` (an instance
/// name: dots become `-`, control characters are dropped, and it is cut
/// to [`MAX_NAME_BYTES`]). Returns once the OS has accepted it, or an
/// error (an empty name, port 0, an oversized TXT value, or the OS
/// refusing it, e.g. macOS Local Network access turned off).
pub fn register(name: &str, port: u16, txt: &Txt) -> Result<Registration> {
    let clean_name = instance_name(name);
    if clean_name.is_empty() {
        bail!("name: empty after removing dots and control characters; give a readable name");
    }
    if port == 0 {
        bail!("port: must be 1 to 65535");
    }
    txt.validate()?;
    Ok(Registration {
        inner: sys::register(&clean_name, port, txt)?,
    })
}

/// Follow engines on the network: `on_event` gets every engine already
/// there as `Found`, then changes as they happen, until the [`Watch`]
/// drops. Engines whose service resolves but has no address yet aren't
/// reported until one arrives.
pub fn watch(on_event: OnEvent) -> Result<Watch> {
    Ok(Watch {
        _inner: sys::watch(on_event)?,
    })
}

/// The engines that answer within `timeout`, sorted by name. macOS
/// answers in well under a second; Windows confirms each engine with a
/// fresh query, which took 0.1 to 3 s on a LAN. Returns by the deadline
/// plus the watch's shutdown however much the network sends: both
/// backends stop between replies, even inside one OS call (macOS ends the
/// operation from its callback), so shutdown takes at most about one
/// 200 ms poll interval, assuming the OS's DNS-SD service answers its
/// calls promptly. An error if browsing itself failed.
pub fn browse(timeout: Duration) -> Result<Vec<Found>> {
    let (send, receive) = mpsc::channel();
    let watching = watch(Box::new(move |event| {
        let _ = send.send(event);
    }))?;
    let deadline = Instant::now() + timeout;
    let mut found = BTreeMap::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match receive.recv_timeout(left) {
            Ok(Event::Found(engine)) => {
                found.insert(engine.name.clone(), engine);
            }
            Ok(Event::Lost { name }) => {
                found.remove(&name);
            }
            Ok(Event::Error(reason)) => {
                drop(watching);
                bail!("could not look for engines: {reason}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            // The watch thread ended; it reports why itself.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(watching);
    Ok(found.into_values().collect())
}

/// `name` as a DNS-SD instance name every backend handles the same way.
/// Windows splits service names at dots, so they become `-` everywhere.
fn instance_name(name: &str) -> String {
    let replaced: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '.' { '-' } else { c })
        .collect();
    truncate(replaced.trim(), MAX_NAME_BYTES).to_owned()
}

/// Untrusted text from the network: control characters dropped, cut to
/// `max` bytes on a character boundary.
pub(crate) fn clean(text: &str, max: usize) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    truncate(&text, max).to_owned()
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Sort and dedupe addresses: IPv4 first, then IPv6.
pub(crate) fn sort_addrs(addrs: &mut Vec<SocketAddr>) {
    addrs.sort_by_key(|addr| (addr.is_ipv6(), *addr));
    addrs.dedup();
}

/// A host name without its trailing dot, bounded.
pub(crate) fn clean_host(host: &str) -> String {
    clean(host.trim_end_matches('.'), 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_cleaned() {
        assert_eq!(instance_name("  exon.desk\n "), "exon-desk");
        assert_eq!(instance_name("..."), "---");
        assert_eq!(instance_name("\u{7}"), "");
        let long = "é".repeat(40);
        let name = instance_name(&long);
        assert!(name.len() <= MAX_NAME_BYTES);
        assert!(name.chars().all(|c| c == 'é'));
    }

    #[test]
    fn untrusted_text_is_bounded() {
        assert_eq!(clean("a\u{0}b\u{1b}[2Jc", 255), "ab[2Jc");
        assert_eq!(clean("ééé", 3), "é");
        assert_eq!(clean_host("Exon-Desk.local."), "Exon-Desk.local");
    }

    #[test]
    fn received_txt_keeps_only_known_keys() {
        let mut txt = Txt::default();
        txt.set("ID", "abc");
        txt.set("version", "9.9");
        txt.set("name", &"n".repeat(500));
        assert_eq!(txt.id, "abc");
        assert_eq!(txt.name.len(), MAX_TXT_VALUE_BYTES);
        assert!(txt.validate().is_ok());
        txt.proto = "1\n".into();
        assert!(txt.validate().is_err());
    }
}
