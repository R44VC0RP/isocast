//! The link to isocastd, through `isocast-client` as the CLI uses it: a
//! worker thread that makes calls in order (any method of the API, by
//! [`Method`]), and a watcher thread that follows the daemon's events and
//! reconnects when it goes away. Nothing here knows what the calls mean;
//! replies and events go back to the window's thread as [`Update`]s, where
//! `App` hands each reply to the closure that asked for it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use isocast_client::{Client, DaemonError};
use isocast_protocol::api::{Event, Live, Method, RpcError};
use serde_json::Value;

/// A call to make, numbered so its reply finds its way back.
#[derive(Debug)]
pub struct Request {
    pub id: u64,
    pub method: Method,
}

/// Why a call failed: the daemon's refusal (with its JSON-RPC code), or no
/// daemon to ask (`code` is `None`).
#[derive(Debug, Clone)]
pub struct CallError {
    pub message: String,
    pub code: Option<i64>,
}

impl CallError {
    /// The setup changed since the revision the call was made against.
    pub fn is_stale(&self) -> bool {
        self.code == Some(RpcError::WRONG_STATE) && self.message.contains("revision")
    }

    /// This daemon doesn't have the method (an older isocastd).
    pub fn is_unknown_method(&self) -> bool {
        self.code == Some(RpcError::METHOD_NOT_FOUND)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The state of the link, as the window shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Connected,
    /// It was connected and went away; reconnecting.
    Disconnected,
    /// It couldn't be reached at all (not launched, or refused).
    Failed,
    /// A remote engine answered with another version (decision 7): it
    /// can't be controlled until the two match. The reason names both.
    Mismatch,
    /// A remote engine presented a key other than the one paired with: it
    /// was reinstalled (or something else answers at its address), so it
    /// must be paired again.
    IdentityChanged,
}

/// What comes back to the window's thread.
#[derive(Debug)]
pub enum Update {
    /// The reply to call `id`.
    Reply {
        id: u64,
        result: Result<Value, CallError>,
    },
    /// The link changed (with why, when it isn't connected).
    Link(Link, String),
    /// Watching (again): the live setup as it is now. A daemon that
    /// restarted numbers its revisions afresh.
    Watching(Box<Live>),
    /// An event from the daemon.
    Event(Box<Event>),
    /// A remote engine said who it is (`hello`): its name, operating system
    /// and version, on each connection of the window's watcher.
    Hello(Box<isocast_protocol::hello::Hello>),
}

/// Start the worker and the watcher. `deliver` runs on their threads and
/// must hand the update to the window's thread.
/// Set while an app update finishes (src/update.rs): isocastd was stopped
/// so the new app starts its own, so this one doesn't launch it again.
pub static NO_LAUNCH: AtomicBool = AtomicBool::new(false);

/// Counts the watcher's connections: when it has connected again, the
/// engine (or the link to it) went away meanwhile, so the worker's own
/// connection is dead too and is replaced before the next call, instead of
/// failing that call (a pulled cable to a remote engine, a restart).
static LINK_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The watcher is connected now (its `watch` is live).
static LINK_UP: AtomicBool = AtomicBool::new(false);

/// Whether the window's link to its engine is up now, and its generation
/// (it changes each time the watcher connects again): a stream opened on
/// another generation went with the link it began on.
pub fn link() -> (bool, u64) {
    (
        LINK_UP.load(Ordering::SeqCst),
        LINK_GENERATION.load(Ordering::SeqCst),
    )
}

/// A connection to the window's engine (src/engine.rs). Only this
/// machine's engine is ever launched; another computer's is connected to,
/// or not.
pub fn connect() -> anyhow::Result<Client> {
    if !crate::engine::is_local() {
        let engine = crate::engine::current();
        return engine
            .connect()?
            .ok_or_else(|| anyhow::anyhow!("the engine isn't reachable"));
    }
    if NO_LAUNCH.load(Ordering::SeqCst) {
        Client::connect()?.ok_or_else(|| anyhow::anyhow!("isocastd was stopped for an update"))
    } else {
        Client::connect_or_launch()
    }
}

/// Start the preview's existing engine path while the window opens its GPU.
/// The short-lived subscription is deliberately not an `engine` hold: it
/// cannot leave capture pinned if the app exits or window creation fails.
#[cfg(windows)]
pub fn prewarm_preview() {
    // Shared surfaces only exist for this machine's engine.
    if !crate::engine::is_local() {
        return;
    }
    let _ = thread::Builder::new()
        .name("preview preparation".into())
        .spawn(|| {
            let result = connect().and_then(|client| {
                client.shared_frames(isocast_protocol::api::SharedFramesParams {
                    target: "program".into(),
                    max_width: Some(1),
                    max_height: Some(1),
                    fps: Some(1),
                    slots: Some(2),
                    filtered: false,
                })
            });
            // No pixels are fetched or read. Dropping the stream closes its
            // pipe; normal preview subscribers take over, or the engine idles.
            if let Err(error) = result {
                isocast_diagnostics::diagnostic!("debug";
                    "preview preparation: {error:#}"
                );
            }
        });
}

pub fn start(deliver: impl Fn(Update) + Send + Clone + 'static) -> Sender<Request> {
    let (tx, rx) = channel();
    let worker_deliver = deliver.clone();
    thread::Builder::new()
        .name("isocastd calls".into())
        .spawn(move || worker(rx, worker_deliver))
        .expect("spawn a thread");
    thread::Builder::new()
        .name("isocastd events".into())
        .spawn(move || watcher(deliver))
        .expect("spawn a thread");
    tx
}

fn worker(rx: Receiver<Request>, deliver: impl Fn(Update)) {
    let mut client: Option<(Client, u64)> = None;
    while let Ok(request) = rx.recv() {
        // Offline previews live in this process. Never regenerate one for
        // Send (or retry a send whose reply may have been lost).
        if crate::engine::is_local()
            && let Method::BugReport(params) = &request.method
            && let Some(id) = params
                .preview_id
                .as_deref()
                .and_then(|id| id.strip_prefix("app-"))
        {
            let mut params = params.clone();
            params.preview_id = Some(id.into());
            deliver(Update::Reply {
                id: request.id,
                result: offline_report(&params),
            });
            continue;
        }
        let generation = LINK_GENERATION.load(Ordering::SeqCst);
        let connection = match client.take() {
            Some((c, at)) if at == generation => Ok(c),
            _ => connect(),
        };
        let mut c = match connection {
            Ok(c) => c,
            Err(error) => {
                if crate::engine::is_local()
                    && let Method::BugReport(params) = &request.method
                    && !params.submit
                    && params.preview_id.is_none()
                {
                    deliver(Update::Reply {
                        id: request.id,
                        result: offline_report(params),
                    });
                    continue;
                }
                deliver(Update::Reply {
                    id: request.id,
                    result: Err(CallError {
                        message: format!("Can't reach isocastd: {error:#}"),
                        code: None,
                    }),
                });
                continue;
            }
        };
        let result: anyhow::Result<Value> = c.call(request.method);
        let result = match result {
            Ok(value) => {
                client = Some((c, generation));
                Ok(value)
            }
            Err(error) => match error.downcast_ref::<DaemonError>() {
                // The daemon refused: the connection is still good.
                Some(refusal) => {
                    client = Some((c, generation));
                    Err(CallError {
                        message: refusal.0.message.clone(),
                        code: Some(refusal.code()),
                    })
                }
                None => Err(CallError {
                    message: format!("Lost isocastd: {error:#}"),
                    code: None,
                }),
            },
        };
        deliver(Update::Reply {
            id: request.id,
            result,
        });
    }
}

fn offline_report(
    params: &isocast_protocol::diagnostics::BugReportParams,
) -> Result<Value, CallError> {
    let mut report =
        isocast_diagnostics::bundle::disconnected(params).map_err(|message| CallError {
            message,
            code: None,
        })?;
    report.preview_id = report.preview_id.map(|id| format!("app-{id}"));
    Ok(serde_json::json!(report))
}

/// Follow the daemon's events; reconnect with back-off when it goes away.
/// A remote engine's `hello` goes to the window first; an engine of another
/// version, or one whose identity changed, is shown as such and asked
/// again less often (nothing else may be called on it).
fn watcher(deliver: impl Fn(Update)) {
    let mut delay = Duration::from_millis(250);
    let mut ever_connected = false;
    deliver(Update::Link(Link::Connecting, String::new()));
    loop {
        let mut blocked = None;
        let result = connect().and_then(|client| {
            if let Some(hello) = client.hello() {
                deliver(Update::Hello(Box::new(hello.clone())));
            }
            if let Some(why) = client.compatibility_error() {
                blocked = Some(Link::Mismatch);
                anyhow::bail!("{why}");
            }
            let (live, events) = client.watch()?;
            LINK_GENERATION.fetch_add(1, Ordering::SeqCst);
            LINK_UP.store(true, Ordering::SeqCst);
            delay = Duration::from_millis(250);
            ever_connected = true;
            deliver(Update::Link(Link::Connected, String::new()));
            deliver(Update::Watching(Box::new(live)));
            for event in events {
                deliver(Update::Event(Box::new(event?)));
            }
            Ok(())
        });
        LINK_UP.store(false, Ordering::SeqCst);
        let reason = match &result {
            Ok(()) => "isocastd closed the connection".to_string(),
            Err(error) => format!("{error:#}"),
        };
        if let Err(error) = &result
            && blocked.is_none()
            && isocast_client::remote::identity_changed(error)
        {
            blocked = Some(Link::IdentityChanged);
        }
        let link = match blocked {
            Some(link) => link,
            None if ever_connected => Link::Disconnected,
            None => Link::Failed,
        };
        deliver(Update::Link(link, reason));
        if blocked.is_some() {
            // Retrying at once can't help: another version or key needs a
            // person (an update, or pairing again).
            thread::sleep(Duration::from_secs(10));
            continue;
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}
