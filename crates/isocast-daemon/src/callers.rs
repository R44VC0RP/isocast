//! Who the request being answered on this thread came from
//! (docs/remote-engines.md, decisions 4, 13, 15 and 20): `handle` and the
//! obs-websocket bridge set it for each request, and the methods that
//! treat callers differently read it here — local-only refusals, stream
//! keys cleared when a network caller moves a destination, and the `by`
//! that events carry.

use std::cell::RefCell;

use isocast_protocol::caller::Caller;
use isocast_protocol::hello::{BuildIdentity, EngineOs, Hello, PROTOCOL, Provenance};

use super::*;

thread_local! {
    static CALLER: RefCell<Caller> = RefCell::new(Caller::local());
}

/// Marks this thread as answering `caller`'s request until dropped (then
/// it's back to whoever it answered before: local, for threads that
/// answer nobody's).
pub(crate) struct Entered(Option<Caller>);

pub(crate) fn enter(caller: Caller) -> Entered {
    Entered(Some(CALLER.with(|current| current.replace(caller))))
}

impl Drop for Entered {
    fn drop(&mut self) {
        if let Some(before) = self.0.take() {
            CALLER.with(|current| *current.borrow_mut() = before);
        }
    }
}

/// The caller this thread is answering.
pub(crate) fn current() -> Caller {
    CALLER.with(|current| current.borrow().clone())
}

/// The name events give the caller (`by`): a network caller's, else none.
pub(crate) fn by() -> Option<String> {
    CALLER.with(|current| current.borrow().by())
}

/// Why the caller may not call `method` here, as `NOT_PERMITTED`.
pub(crate) fn refusal(caller: &Caller, method: &Method) -> Option<RpcError> {
    let message = caller.refusal(method, machine_name())?;
    Some(RpcError {
        code: RpcError::NOT_PERMITTED,
        message,
    })
}

/// `message` (a refusal after keys were cleared) with the keys already
/// cleared, so nobody is told nothing changed when keys are gone.
pub(crate) fn with_cleared(message: String, cleared: &[String]) -> String {
    if cleared.is_empty() {
        return message;
    }
    format!(
        "{message} (the change wasn't made, but these stream keys were already cleared and must \
         be set again: {})",
        cleared.join("; ")
    )
}

/// The method's name, as requests spell it.
pub(crate) fn method_name(method: &Method) -> String {
    serde_json::to_value(method)
        .ok()
        .and_then(|value| value.get("method")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl Daemon {
    /// Log and publish a network caller's refused request (decision 15):
    /// any `NOT_PERMITTED` answer to a caller that isn't local.
    pub(crate) fn note_refusal(&self, caller: &Caller, method: &str, error: &RpcError) {
        if caller.is_local() || error.code != RpcError::NOT_PERMITTED {
            return;
        }
        isocast_diagnostics::diagnostic!("warn";
            "isocastd: refused {method} from {}: {}",
            caller.by().unwrap_or_default(),
            error.message
        );
        self.publish(Event::Refused {
            method: method.to_string(),
            message: error.message.clone(),
            by: caller.by(),
        });
    }
}

/// This machine's name as people know it ("exon-desk", "Ryan's MacBook
/// Pro"), for "change this on …" and `hello`.
pub(crate) fn machine_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let name = computer_name()
            .or_else(host_name)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        name.unwrap_or_else(|| format!("this {}", isocast_protocol::platform_text::MACHINE))
    })
}

#[cfg(target_os = "macos")]
fn computer_name() -> Option<String> {
    // The name in System Settings › General › Sharing.
    let out = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
fn computer_name() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn computer_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
}

#[cfg(unix)]
fn host_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its length, and gethostname writes at
    // most that many bytes.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return None;
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    Some(name.strip_suffix(".local").unwrap_or(&name).to_string())
}

#[cfg(not(unix))]
fn host_name() -> Option<String> {
    None
}

/// `hello`'s answer.
pub(crate) fn hello() -> Hello {
    Hello {
        os: EngineOs::current(),
        // The remote lane gives the engine its identity.
        engine_id: None,
        name: machine_name().to_string(),
        build: build_identity(),
        protocol: PROTOCOL,
        capabilities: isocast_protocol::api::method_names().to_vec(),
    }
}

/// This engine's build, as `hello` gives it: product version, commit and
/// provenance (build.rs: `ISOCAST_BUILD_PROVENANCE`), the same shape on
/// every operating system.
pub(crate) fn build_identity() -> BuildIdentity {
    let daemon = daemon_build();
    BuildIdentity {
        version: daemon.version,
        commit: daemon.commit,
        provenance: Provenance::from_build(option_env!("ISOCAST_BUILD_PROVENANCE")),
    }
}

/// A recording session's id: a random id for this run of the engine and
/// the session's number, so a stop retried after a restart (or against
/// another engine) never matches a new session.
pub(crate) fn session_token(number: u64) -> String {
    static RUN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let run = RUN.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        // Keys from the operating system's random source (std seeds
        // RandomState from it), mixed with the time and process id.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos()),
        );
        hasher.write_u32(std::process::id());
        hasher.finish()
    });
    format!("{run:016x}-{number}")
}

/// Every stream destination of `after` whose server or service isn't what
/// it was in `before` (or that is new), with its Keychain account then.
fn moved_destinations(
    before: Option<&Setup>,
    after: Option<&Setup>,
) -> Vec<(CanvasId, String, String)> {
    let Some(after) = after else {
        return Vec::new();
    };
    let mut moved = Vec::new();
    for canvas in after.canvas_ids() {
        let Some(settings) = after.stream_of(canvas) else {
            continue;
        };
        let old = before
            .and_then(|before| before.stream_of(canvas))
            .map(|settings| settings.resolve_canvas(canvas))
            .unwrap_or_default();
        for destination in settings.resolve_canvas(canvas) {
            let same = old.iter().any(|old| {
                old.id == destination.id
                    && old.settings.service == destination.settings.service
                    && old.settings.server == destination.settings.server
            });
            if !same {
                moved.push((canvas, destination.id, destination.key_account));
            }
        }
    }
    moved
}

impl Daemon {
    /// Decision 13, failing closed (decision 25): when a network caller
    /// changes a destination's server or service (or adds one), every
    /// credential it would publish with is removed before the change is
    /// made: the key stored for it, and one an older Isocast stored under
    /// the legacy service (it would be moved over and used). Refused
    /// instead, with nothing changed, when that can't be done or checked,
    /// or when `ISOCAST_STREAM_KEY` (which every destination uses) is set.
    /// Local callers keep their keys. Returns the notes for the reply; the
    /// caller publishes them as warning events once the state lock is let
    /// go. Called holding the state lock, after everything else that can
    /// refuse the change, so no stream can start on the new server with
    /// the old key.
    pub(crate) fn clear_moved_keys(
        &self,
        before: Option<&Setup>,
        after: Option<&Setup>,
    ) -> Result<Vec<String>, RpcError> {
        let caller = current();
        if caller.is_local() {
            return Ok(Vec::new());
        }
        let moved = moved_destinations(before, after);
        if moved.is_empty() {
            return Ok(Vec::new());
        }
        let machine = machine_name();
        let refuse = |message: String| RpcError {
            code: RpcError::NOT_PERMITTED,
            message,
        };
        if std::env::var(KEY_ENV).is_ok_and(|key| !key.trim().is_empty()) {
            let (_, id, _) = &moved[0];
            return Err(refuse(format!(
                "stream destination {id:?}: its server or service can't change from another \
                 computer while {KEY_ENV} is set in the engine's environment (every destination \
                 streams with it, and it can't be cleared); change it on {machine}"
            )));
        }
        let who = caller.by().unwrap_or_default();
        let mut services = vec![key_service()];
        if key_service() != LEGACY_KEY_SERVICE {
            services.push(LEGACY_KEY_SERVICE);
        }
        let label = |canvas: CanvasId, id: &str| {
            if canvas.is_main() {
                format!("stream destination {id:?}")
            } else {
                format!("stream destination {id:?} on canvas {canvas}")
            }
        };
        // First every check, so a check that fails changes nothing.
        let mut doomed = Vec::new();
        for (canvas, id, account) in &moved {
            for service in &services {
                let stored = has_secret(service, account).map_err(|err| {
                    refuse(format!(
                        "{}: its server or service changes, but whether a stream key is stored \
                         for it couldn't be checked ({err:#}); nothing changed. Change it on \
                         {machine}",
                        label(*canvas, id)
                    ))
                })?;
                if stored {
                    doomed.push((*canvas, id.clone(), account.clone(), *service));
                }
            }
        }
        // Then the deletions; one that fails says truthfully which keys
        // are already gone (the setup itself isn't changed).
        let mut cleared: Vec<(CanvasId, String)> = Vec::new();
        for (canvas, id, account, service) in doomed {
            if let Err(err) = delete_secret(service, &account) {
                let gone: Vec<String> = cleared.iter().map(|(c, i)| label(*c, i)).collect();
                return Err(refuse(format!(
                    "{}: its server or service changes, but its stream key couldn't be cleared \
                     ({err:#}), so the change wasn't made{}. Clear it or change the destination \
                     on {machine}",
                    label(canvas, &id),
                    if gone.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; the stream keys of {} were already cleared and must be set again",
                            gone.join(", ")
                        )
                    }
                )));
            }
            if !cleared.iter().any(|(c, i)| *c == canvas && *i == id) {
                cleared.push((canvas, id));
            }
        }
        let mut notes = Vec::new();
        for (canvas, id) in cleared {
            let canvas_flag = if canvas.is_main() {
                String::new()
            } else {
                format!(" --canvas {canvas}")
            };
            let note = format!(
                "{}: its stream key was cleared because {who} changed its server or service \
                 from another computer; set the key again (`isocast stream key set \
                 --destination {id}{canvas_flag}`) before streaming there",
                label(canvas, &id)
            );
            isocast_diagnostics::diagnostic!("warn"; "isocastd: {note}");
            notes.push(note);
        }
        Ok(notes)
    }
}
