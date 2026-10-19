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
}

/// Start the worker and the watcher. `deliver` runs on their threads and
/// must hand the update to the window's thread.
/// Set while an app update finishes (src/update.rs): isocastd was stopped
/// so the new app starts its own, so this one doesn't launch it again.
pub static NO_LAUNCH: AtomicBool = AtomicBool::new(false);

pub fn connect() -> anyhow::Result<Client> {
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
    let mut client: Option<Client> = None;
    while let Ok(request) = rx.recv() {
        let connection = match client.take() {
            Some(c) => Ok(c),
            None => connect(),
        };
        let mut c = match connection {
            Ok(c) => c,
            Err(error) => {
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
                client = Some(c);
                Ok(value)
            }
            Err(error) => match error.downcast_ref::<DaemonError>() {
                // The daemon refused: the connection is still good.
                Some(refusal) => {
                    client = Some(c);
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

/// Follow the daemon's events; reconnect with back-off when it goes away.
fn watcher(deliver: impl Fn(Update)) {
    let mut delay = Duration::from_millis(250);
    let mut ever_connected = false;
    deliver(Update::Link(Link::Connecting, String::new()));
    loop {
        let result = connect().and_then(|client| {
            let (live, events) = client.watch()?;
            delay = Duration::from_millis(250);
            ever_connected = true;
            deliver(Update::Link(Link::Connected, String::new()));
            deliver(Update::Watching(Box::new(live)));
            for event in events {
                deliver(Update::Event(Box::new(event?)));
            }
            Ok(())
        });
        let reason = match result {
            Ok(()) => "isocastd closed the connection".to_string(),
            Err(error) => format!("{error:#}"),
        };
        let link = if ever_connected {
            Link::Disconnected
        } else {
            Link::Failed
        };
        deliver(Update::Link(link, reason));
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}
