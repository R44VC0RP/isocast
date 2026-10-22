//! Failed-login limiting (docs/remote-engines.md, decision 14): after a
//! few wrong passwords from one address, that address must wait before it
//! may try again, twice as long after each further failure, so a password
//! can't be guessed at network speed. A good login clears the address.
//!
//! A try is admitted under one lock, as a reservation counted against what
//! the address has left: many connections at once from one address get no
//! more tries than one after another. Reservations aren't failures: only a
//! wrong password counts, and a right one never extends a wait. Addresses
//! are grouped as an attacker controls them (IPv4-mapped IPv6 as IPv4,
//! IPv6 by /64). Every address off this machine together has its own,
//! larger budget, so rotating addresses doesn't help either; loopback
//! clients (this machine's OBS tools) keep their own per-address limit
//! and are never held up by that budget.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Wrong passwords allowed from one address before it has to wait.
pub const FREE_FAILURES: u32 = 5;
/// Wrong passwords allowed from all non-loopback addresses together before
/// every such try has to wait.
pub const FREE_TOTAL: u32 = 30;
/// The first wait; each further failure doubles it.
pub const FIRST_WAIT: Duration = Duration::from_secs(1);
/// The longest wait.
pub const MAX_WAIT: Duration = Duration::from_secs(300);
/// A count is forgotten this long after its last failure.
const FORGET_AFTER: Duration = Duration::from_secs(15 * 60);
/// Addresses remembered at most; the stalest idle ones go first.
const MAX_ADDRESSES: usize = 4096;

/// What a try's address counts under.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    V4([u8; 4]),
    /// The /64 prefix.
    V6([u16; 4]),
}

impl Key {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Self::V4(v4.octets()),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self::V4(v4.octets()),
                None => {
                    let s = v6.segments();
                    Self::V6([s[0], s[1], s[2], s[3]])
                }
            },
        }
    }

    fn loopback(self) -> bool {
        match self {
            Self::V4(octets) => octets[0] == 127,
            Self::V6(segments) => segments == [0, 0, 0, 0],
        }
    }
}

#[derive(Clone, Copy)]
struct Failures {
    count: u32,
    last: Instant,
    /// No try is admitted before this.
    until: Option<Instant>,
    /// Tries admitted and not yet answered.
    in_flight: u32,
}

impl Failures {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            last: now,
            until: None,
            in_flight: 0,
        }
    }

    fn forget_if_stale(&mut self, now: Instant) {
        if now.duration_since(self.last) > FORGET_AFTER {
            self.count = 0;
            self.until = None;
        }
    }

    fn wait(&self, now: Instant) -> Option<Duration> {
        self.until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|wait| !wait.is_zero())
    }

    /// Tries that may be admitted now: what's left of `free`, less those
    /// in flight; one at a time once it's spent.
    fn admits(&self, free: u32) -> bool {
        let left = free.saturating_sub(self.count).max(1);
        self.in_flight < left
    }

    fn fail(&mut self, now: Instant, free: u32) {
        self.forget_if_stale(now);
        self.count = self.count.saturating_add(1);
        self.last = now;
        self.until = match self.count.checked_sub(free) {
            Some(over) if over > 0 => Some(
                now + FIRST_WAIT
                    .checked_mul(1u32 << (over - 1).min(16))
                    .unwrap_or(MAX_WAIT)
                    .min(MAX_WAIT),
            ),
            _ => None,
        };
    }
}

/// An admitted try; give it back with [`AuthLimit::finish`].
#[derive(Debug)]
pub struct Ticket(Key);

pub struct AuthLimit {
    by_address: HashMap<Key, Failures>,
    /// Every non-loopback address together.
    total: Failures,
}

impl Default for AuthLimit {
    fn default() -> Self {
        Self {
            by_address: HashMap::new(),
            total: Failures::new(Instant::now()),
        }
    }
}

impl AuthLimit {
    /// Admit a try from `ip`, or refuse it with the wait left.
    pub fn attempt(&mut self, ip: IpAddr, now: Instant) -> Result<Ticket, Duration> {
        let key = Key::of(ip);
        if !key.loopback() {
            self.total.forget_if_stale(now);
            if let Some(wait) = self.total.wait(now) {
                return Err(wait);
            }
            if !self.total.admits(FREE_TOTAL) {
                return Err(FIRST_WAIT);
            }
        }
        if self.by_address.len() >= MAX_ADDRESSES && !self.by_address.contains_key(&key) {
            self.forget_stalest(now);
        }
        let entry = self
            .by_address
            .entry(key)
            .or_insert_with(|| Failures::new(now));
        entry.forget_if_stale(now);
        if let Some(wait) = entry.wait(now) {
            return Err(wait);
        }
        if !entry.admits(FREE_FAILURES) {
            return Err(FIRST_WAIT);
        }
        entry.in_flight += 1;
        if !key.loopback() {
            self.total.in_flight += 1;
        }
        Ok(Ticket(key))
    }

    /// The admitted try's answer: a wrong password counts as a failure; a
    /// right one clears the address and never extends a wait.
    pub fn finish(&mut self, ticket: Ticket, good: bool, now: Instant) {
        let key = ticket.0;
        if !key.loopback() {
            self.total.in_flight = self.total.in_flight.saturating_sub(1);
            if !good {
                self.total.fail(now, FREE_TOTAL);
            }
        }
        if let Some(entry) = self.by_address.get_mut(&key) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            if good {
                entry.count = 0;
                entry.until = None;
            } else {
                entry.fail(now, FREE_FAILURES);
            }
            if entry.count == 0 && entry.in_flight == 0 {
                self.by_address.remove(&key);
            }
        }
    }

    fn forget_stalest(&mut self, now: Instant) {
        self.by_address.retain(|_, failures| {
            failures.in_flight > 0 || now.duration_since(failures.last) <= FORGET_AFTER
        });
        if self.by_address.len() >= MAX_ADDRESSES
            && let Some(stalest) = self
                .by_address
                .iter()
                .filter(|(_, failures)| failures.in_flight == 0)
                .min_by_key(|(_, failures)| failures.last)
                .map(|(key, _)| *key)
        {
            self.by_address.remove(&stalest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_tries_and_waits() {
        let mut limit = AuthLimit::default();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let now = Instant::now();
        // At once: only the free tries are admitted.
        let tickets: Vec<_> = (0..20)
            .filter_map(|_| limit.attempt(ip, now).ok())
            .collect();
        assert_eq!(tickets.len(), FREE_FAILURES as usize);
        for ticket in tickets {
            limit.finish(ticket, false, now);
        }
        // Spent: one more, alone, then a wait.
        let t = limit.attempt(ip, now).unwrap();
        assert!(limit.attempt(ip, now).is_err());
        limit.finish(t, false, now);
        assert_eq!(limit.attempt(ip, now).unwrap_err(), Duration::from_secs(1));
        // The mapped form is the same address.
        assert!(
            limit
                .attempt("::ffff:10.0.0.9".parse().unwrap(), now)
                .is_err()
        );
        // A right password clears it.
        let later = now + Duration::from_secs(1);
        let t = limit.attempt(ip, later).unwrap();
        limit.finish(t, true, later);
        assert!(limit.attempt(ip, later).is_ok());
    }

    #[test]
    fn successes_never_count() {
        let mut limit = AuthLimit::default();
        let now = Instant::now();
        for i in 0..200 {
            let ip: IpAddr = format!("10.2.{}.{}", i / 200, i % 200).parse().unwrap();
            let t = limit.attempt(ip, now).unwrap();
            limit.finish(t, true, now);
        }
        assert!(limit.attempt("10.9.9.9".parse().unwrap(), now).is_ok());
    }

    #[test]
    fn rotating_addresses_hit_the_total_but_not_loopback() {
        let mut limit = AuthLimit::default();
        let now = Instant::now();
        for i in 0..FREE_TOTAL + 1 {
            let ip: IpAddr = format!("10.1.{}.{}", i / 250, i % 250).parse().unwrap();
            if let Ok(t) = limit.attempt(ip, now) {
                limit.finish(t, false, now);
            }
        }
        assert!(limit.attempt("10.9.9.9".parse().unwrap(), now).is_err());
        // This machine's own tools aren't held up by strangers.
        assert!(limit.attempt("127.0.0.1".parse().unwrap(), now).is_ok());
        // One /64 is one address.
        let mut limit = AuthLimit::default();
        for i in 0..FREE_FAILURES {
            let t = limit
                .attempt(format!("2001:db8::{:x}", i + 1).parse().unwrap(), now)
                .unwrap();
            limit.finish(t, false, now);
        }
        let t = limit
            .attempt("2001:db8::ffff".parse().unwrap(), now)
            .unwrap();
        limit.finish(t, false, now);
        assert!(
            limit
                .attempt("2001:db8::abcd".parse().unwrap(), now)
                .is_err()
        );
    }
}
