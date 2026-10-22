//! The frozen pairing exchange is a bounded binary prelude, not JSON-RPC.
//! Unpaired TLS peers never enter the ordinary request parser. Both endpoints
//! confirm the SPAKE2 transcript, both RPKs and this connection's TLS exporter.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use zeroize::Zeroizing;

use crate::tls::fingerprint;
use crate::{Duplex, Identity};

const PRELUDE: &[u8] = b"isocast/1 remote_pair_finish\n";
const WINDOW: Duration = Duration::from_secs(120);
const EXCHANGE: Duration = Duration::from_secs(10);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) fn requested(stream: &Duplex) -> io::Result<bool> {
    // The fixed prelude starts with 'i', which cannot start a JSON-RPC
    // object. finish() still validates the entire prelude before any PAKE.
    Ok(stream.peek_byte(Duration::from_secs(2))? == Some(PRELUDE[0]))
}

#[derive(Default)]
pub struct Pairing(Mutex<State>);

#[derive(Default)]
struct State {
    window: Option<Window>,
    pending: Option<Arc<AtomicBool>>,
    failures: Vec<Instant>,
    cooldown: Option<Instant>,
}

struct Window {
    code: Zeroizing<String>,
    expires: Instant,
}

pub struct Attempt {
    pairing: Arc<Pairing>,
    code: Zeroizing<String>,
    deadline: Instant,
    success: bool,
    cancelled: Arc<AtomicBool>,
}

impl Pairing {
    /// Only an engine-local caller may invoke this. Reopening explicitly
    /// clears cooldown, but cannot replace an exchange already in progress.
    pub fn begin(&self) -> io::Result<Zeroizing<String>> {
        let mut state = self.0.lock().unwrap();
        if state.pending.is_some() {
            return Err(io::Error::other("a pairing attempt is in progress"));
        }
        let mut bytes = [0u8; 4];
        let value = loop {
            SystemRandom::new()
                .fill(&mut bytes)
                .map_err(|_| io::Error::other("OS random generator unavailable"))?;
            let value = u32::from_le_bytes(bytes);
            // Rejection sampling: exactly one million equiprobable values.
            if value < u32::MAX - (u32::MAX % 1_000_000) {
                break value % 1_000_000;
            }
        };
        let code = Zeroizing::new(format!("{value:06}"));
        state.window = Some(Window {
            code: code.clone(),
            expires: Instant::now() + WINDOW,
        });
        state.cooldown = None;
        Ok(code)
    }

    pub fn is_open(&self) -> bool {
        let state = self.0.lock().unwrap();
        let now = Instant::now();
        state.pending.is_none()
            && state.cooldown.is_none_or(|until| until <= now)
            && state
                .window
                .as_ref()
                .is_some_and(|window| window.expires > now)
    }

    pub fn close(&self) {
        let mut state = self.0.lock().unwrap();
        state.window = None;
        if let Some(cancelled) = &state.pending {
            cancelled.store(true, Ordering::Release);
        }
    }

    /// Reserve and burn the code atomically before reading any application
    /// bytes. A parallel connection cannot reuse it, even after disconnect.
    pub fn claim(self: &Arc<Self>) -> io::Result<Attempt> {
        let mut state = self.0.lock().unwrap();
        let now = Instant::now();
        if state.pending.is_some() || state.cooldown.is_some_and(|until| until > now) {
            return Err(io::Error::other("pairing is closed"));
        }
        let window = state
            .window
            .take()
            .filter(|window| window.expires > now)
            .ok_or_else(|| io::Error::other("pairing is closed; open a new window locally"))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        state.pending = Some(cancelled.clone());
        Ok(Attempt {
            pairing: self.clone(),
            code: window.code,
            deadline: window.expires.min(now + EXCHANGE),
            success: false,
            cancelled,
        })
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        let mut state = self.pairing.0.lock().unwrap();
        state.pending = None;
        if !self.success {
            let now = Instant::now();
            state.failures.retain(|at| now.duration_since(*at) < DAY);
            state.failures.push(now);
            if state.failures.len() > 20 {
                state.failures.remove(0);
            }
            if state.failures.len() >= 20
                || state
                    .failures
                    .iter()
                    .filter(|at| now.duration_since(**at) < WINDOW)
                    .count()
                    >= 3
            {
                state.cooldown = Some(now + Duration::from_secs(60));
            }
        }
    }
}

fn timeout(stream: &Duplex, deadline: Instant) -> io::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "pairing expired"))?;
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))
}

// read_exact would reset a read timeout for every partial read. Recompute it
// against the absolute exchange deadline instead.
fn read(stream: &mut Duplex, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        timeout(stream, deadline)?;
        let count = stream.read(bytes)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes = &mut bytes[count..];
    }
    Ok(())
}

fn transcript(
    client: &[u8],
    engine: &[u8],
    exporter: &[u8],
    a: &[u8],
    b: &[u8],
    name: &[u8],
) -> Vec<u8> {
    let mut bytes = b"isocast-pair-v1".to_vec();
    for part in [client, engine, exporter, a, b, name] {
        bytes.extend_from_slice(&(part.len() as u32).to_be_bytes());
        bytes.extend_from_slice(part);
    }
    bytes
}

fn confirmation(key: &[u8], side: &[u8], transcript: &[u8]) -> hmac::Key {
    // Domain-separated confirmation keys avoid reflecting one party's MAC.
    let master = hmac::Key::new(hmac::HMAC_SHA256, key);
    let mut context = hmac::Context::with_key(&master);
    context.update(b"isocast-pair-v1 confirmation");
    context.update(side);
    context.update(transcript);
    hmac::Key::new(hmac::HMAC_SHA256, context.sign().as_ref())
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "pairing confirmation failed; open a new window locally",
    )
}

impl Attempt {
    /// Pin persistence is the callback's responsibility. It runs only after
    /// client confirmation. A persistence failure never sends a success reply.
    pub fn finish(
        mut self,
        stream: &mut Duplex,
        engine: &Identity,
        persist: impl FnOnce(&[u8], &str) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut prelude = vec![0; PRELUDE.len()];
        read(stream, &mut prelude, self.deadline)?;
        if prelude != PRELUDE {
            return Err(refused());
        }
        let mut length = [0u8; 1];
        read(stream, &mut length, self.deadline)?;
        if length[0] == 0 || length[0] > 128 {
            return Err(refused());
        }
        let mut name = vec![0; length[0] as usize];
        read(stream, &mut name, self.deadline)?;
        let name = std::str::from_utf8(&name)
            .ok()
            .filter(|name| !name.chars().any(char::is_control))
            .ok_or_else(refused)?;
        let mut a = [0u8; 33];
        read(stream, &mut a, self.deadline)?;
        let client_id = &stream.peer().client_id;
        let engine_id = engine.id();
        let (spake, b) = Spake2::<Ed25519Group>::start_b(
            &Password::new(self.code.as_bytes()),
            &SpakeIdentity::new(client_id.as_bytes()),
            &SpakeIdentity::new(engine_id.as_bytes()),
        );
        let key = Zeroizing::new(spake.finish(&a).map_err(|_| refused())?);
        let transcript = transcript(
            &stream.peer().spki,
            engine.spki(),
            stream.exporter(),
            &a,
            &b,
            name.as_bytes(),
        );
        let server = confirmation(&key, b"engine", &transcript);
        let client = confirmation(&key, b"client", &transcript);
        timeout(stream, self.deadline)?;
        stream.write_all(&b)?;
        stream.write_all(hmac::sign(&server, &transcript).as_ref())?;
        stream.flush()?;
        let mut proof = [0u8; 32];
        read(stream, &mut proof, self.deadline)?;
        hmac::verify(&client, &transcript, &proof).map_err(|_| refused())?;
        let commit = self.pairing.0.lock().unwrap();
        if Instant::now() >= self.deadline || self.cancelled.load(Ordering::Acquire) {
            return Err(refused());
        }
        persist(&stream.peer().spki, name)?;
        self.success = true;
        drop(commit);
        stream.write_all(&[1])?;
        stream.flush()
    }
}

/// Pair over a dedicated connection, then reconnect with ordinary pinned
/// verifiers. Never promote the unauthenticated connection to a control one.
pub fn finish(
    stream: &mut Duplex,
    client: &Identity,
    name: &str,
    code: &str,
) -> io::Result<Vec<u8>> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(io::Error::other("code: expected six digits"));
    }
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(io::Error::other(
            "name: expected 1–128 UTF-8 bytes without control characters",
        ));
    }
    let deadline = Instant::now() + EXCHANGE;
    let client_id = client.id();
    let engine_id = fingerprint(&stream.peer().spki);
    let (spake, a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &SpakeIdentity::new(client_id.as_bytes()),
        &SpakeIdentity::new(engine_id.as_bytes()),
    );
    timeout(stream, deadline)?;
    stream.write_all(PRELUDE)?;
    stream.write_all(&[name.len() as u8])?;
    stream.write_all(name.as_bytes())?;
    stream.write_all(&a)?;
    stream.flush()?;
    let mut b = [0u8; 33];
    let mut proof = [0u8; 32];
    read(stream, &mut b, deadline)?;
    read(stream, &mut proof, deadline)?;
    let key = Zeroizing::new(spake.finish(&b).map_err(|_| refused())?);
    let transcript = transcript(
        client.spki(),
        &stream.peer().spki,
        stream.exporter(),
        &a,
        &b,
        name.as_bytes(),
    );
    let server = confirmation(&key, b"engine", &transcript);
    let client = confirmation(&key, b"client", &transcript);
    hmac::verify(&server, &transcript, &proof).map_err(|_| refused())?;
    timeout(stream, deadline)?;
    stream.write_all(hmac::sign(&client, &transcript).as_ref())?;
    stream.flush()?;
    let mut accepted = [0u8; 1];
    read(stream, &mut accepted, deadline)?;
    if accepted != [1] {
        return Err(refused());
    }
    Ok(stream.peer().spki.clone())
}
