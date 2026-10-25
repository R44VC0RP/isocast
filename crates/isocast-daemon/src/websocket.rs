//! The obs-websocket v5 compatibility server (#71) in isocastd:
//! `websocket_status`, `websocket_enable` and `websocket_disable`, its
//! settings file and Keychain password, and the bridge that lets the
//! server (crate isocast-obsws) call the method table and watch events
//! like any other client.
//!
//! The settings are this engine's, not a profile's (OBS keeps them per
//! install too): `<setup>.websocket.json` beside the live setup, so test
//! engines on their own socket keep their own. The password is a Keychain
//! generic password under this build's service (`<app id>.websocket`), or
//! `ISOCAST_WEBSOCKET_PASSWORD` for tests.

use std::net::{IpAddr, SocketAddr};
use std::sync::Weak;

use isocast_obsws::{Config, Engine, OBS_WEBSOCKET_VERSION, RPC_VERSION, Server, Watching};
use isocast_protocol::websocket::{WebsocketEnableParams, WebsocketSettings, WebsocketStatus};

use super::*;

/// Overrides the Keychain password (tests, CI).
const PASSWORD_ENV: &str = "ISOCAST_WEBSOCKET_PASSWORD";

#[derive(Default)]
pub(super) struct Runtime {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    settings: WebsocketSettings,
    server: Option<Server>,
    last_error: Option<String>,
}

fn settings_path() -> PathBuf {
    isocast_protocol::api::setup_path().with_extension("websocket.json")
}

fn service() -> String {
    match app_identifier() {
        Some(id) => format!("{id}.websocket"),
        None => "dev.isocast.unbundled.websocket".to_string(),
    }
}

/// The engine on the default socket has one password; a test engine on
/// its own socket has its own.
fn account() -> String {
    let socket = isocast_protocol::api::socket_path();
    if socket == isocast_protocol::api::default_socket_path() {
        "obs-websocket".into()
    } else {
        format!("obs-websocket:{}", socket.display())
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

/// `ISOCAST_WEBSOCKET_PASSWORD`, if set.
fn env_password() -> Option<String> {
    std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty())
}

/// The password clients must give, if any; an error if one should exist
/// but can't be read (the server then stays off).
fn password(settings: &WebsocketSettings) -> Result<Option<String>, String> {
    if !settings.authentication {
        return Ok(None);
    }
    if let Some(password) = env_password() {
        isocast_diagnostics::redact::remember_secret(PASSWORD_ENV, "env", &password);
        return Ok(Some(password));
    }
    match secret(&service(), &account()) {
        Ok(Some(password)) if !password.is_empty() => Ok(Some(password)),
        Ok(_) => Err(
            "no obs-websocket password is stored; set one with `isocast websocket enable \
             --password` (or --no-password)"
                .into(),
        ),
        Err(err) => {
            // The platform's message is about stream keys; say it plainly.
            isocast_diagnostics::diagnostic!("warn"; "isocastd: obs-websocket password: {err:#}");
            Err(
                "could not read the obs-websocket password from the Keychain (another build of \
                 Isocast may have saved it); set it again with `isocast websocket enable \
                 --password`"
                    .into(),
            )
        }
    }
}

impl Runtime {
    /// At launch: load the settings and start the server if it is on.
    pub(super) fn load(&self, daemon: &Arc<Daemon>) {
        let mut inner = self.inner.lock();
        match std::fs::read(settings_path()) {
            Ok(bytes) => match serde_json::from_slice::<WebsocketSettings>(&bytes) {
                Ok(settings) => inner.settings = settings,
                Err(err) => {
                    inner.last_error = Some(format!(
                        "{} didn't load ({err}); obs-websocket stays off until enabled again",
                        settings_path().display()
                    ));
                    return;
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                inner.last_error = Some(format!(
                    "could not read {}: {err}",
                    settings_path().display()
                ));
                return;
            }
        }
        if inner.settings.enabled {
            restart(&mut inner, daemon);
        }
    }

    pub(super) fn status(&self) -> WebsocketStatus {
        status(&self.inner.lock(), None)
    }

    pub(super) fn enable(
        &self,
        daemon: &Arc<Daemon>,
        params: WebsocketEnableParams,
    ) -> Result<WebsocketStatus, RpcError> {
        let mut inner = self.inner.lock();
        let mut settings = inner.settings.clone();
        settings.enabled = true;
        if let Some(port) = params.port {
            settings.port = port;
        }
        if let Some(bind) = params.bind {
            settings.bind = bind;
        }
        if params.no_password && params.password.is_some() {
            return Err(invalid(
                "password: give a password or no_password, not both",
            ));
        }
        // What isn't given stays: a port or address change keeps the
        // authentication as it was (#74); --password turns it back on.
        let authentication = if params.no_password {
            false
        } else if params.password.is_some() {
            true
        } else {
            settings.authentication
        };
        let after = WebsocketSettings {
            authentication,
            ..settings.clone()
        };
        if let Some(problem) = after.problem() {
            return Err(invalid(problem));
        }
        let mut generated = None;
        let mut stored_now = false;
        if params.no_password {
            settings.authentication = false;
            // The stored password goes too; --password sets a new one.
            delete_secret(&service(), &account()).map_err(failed)?;
        } else if let Some(password) = params.password {
            if password.is_empty() {
                return Err(invalid(
                    "password: an empty password; give one, or no_password for none",
                ));
            }
            set_secret(&service(), &account(), &password).map_err(failed)?;
            isocast_diagnostics::redact::remember_secret(&service(), &account(), &password);
            settings.authentication = true;
            stored_now = true;
        } else if settings.authentication && password(&settings).is_err() {
            // On for the first time without a password: make one and show
            // it once.
            let new = isocast_obsws::generate_password().map_err(|err| failed(err.into()))?;
            set_secret(&service(), &account(), &new).map_err(failed)?;
            isocast_diagnostics::redact::remember_secret(&service(), &account(), &new);
            stored_now = true;
            generated = Some(new);
        }
        let warning = if settings.authentication && env_password().is_some() {
            // Clients must give the environment's password, not the
            // Keychain's: say so where the user is looking.
            let stored = stored_now
                || matches!(secret(&service(), &account()), Ok(Some(p)) if !p.is_empty());
            Some(if stored {
                format!(
                    "{PASSWORD_ENV} is set in isocastd's environment and overrides the stored \
                     password: clients must give {PASSWORD_ENV}'s value until it is unset and \
                     isocastd restarted"
                )
            } else {
                format!("clients must give {PASSWORD_ENV}'s value (set in isocastd's environment)")
            })
        } else if !settings.authentication && env_password().is_some() {
            // Not silently (#74): the environment's password does nothing.
            Some(format!(
                "{PASSWORD_ENV} is set in isocastd's environment but the server goes without a \
                 password (no_password), so it is ignored: clients connect without one"
            ))
        } else {
            None
        };
        save(&settings)?;
        inner.settings = settings;
        restart(&mut inner, daemon);
        let mut status = status(&inner, generated);
        status.warning = warning;
        Ok(status)
    }

    pub(super) fn disable(&self) -> Result<WebsocketStatus, RpcError> {
        let mut inner = self.inner.lock();
        let mut settings = inner.settings.clone();
        settings.enabled = false;
        save(&settings)?;
        inner.settings = settings;
        // Stopping says goodbye to the clients first.
        inner.server = None;
        inner.last_error = None;
        Ok(status(&inner, None))
    }

    /// When isocastd quits: tell the clients (ExitStarted) and close.
    pub(super) fn stop(&self) {
        let server = self.inner.lock().server.take();
        drop(server);
    }
}

fn save(settings: &WebsocketSettings) -> Result<(), RpcError> {
    let path = settings_path();
    let partial = path.with_extension("json.partial");
    let bytes = serde_json::to_vec_pretty(settings).map_err(|err| failed(err.into()))?;
    std::fs::write(&partial, bytes)
        .and_then(|()| isocast_protocol::fs::replace(&partial, &path))
        .map_err(|err| failed(anyhow::anyhow!("could not save {}: {err}", path.display())))
}

/// Stop the server if it runs, and start it again with the settings.
fn restart(inner: &mut Inner, daemon: &Arc<Daemon>) {
    inner.server = None;
    inner.last_error = None;
    if !inner.settings.enabled {
        return;
    }
    // Settings saved by an older build may go without a password on
    // every network (#74): stay off until enabled again.
    if let Some(problem) = inner.settings.problem() {
        isocast_diagnostics::diagnostic!("warn"; "isocastd: obs-websocket stays off: {problem}");
        inner.last_error = Some(problem);
        return;
    }
    if !inner.settings.authentication && env_password().is_some() {
        isocast_diagnostics::diagnostic!("warn";
            "isocastd: obs-websocket goes without a password, so {PASSWORD_ENV} is ignored"
        );
    }
    let password = match password(&inner.settings) {
        Ok(password) => password,
        Err(problem) => {
            isocast_diagnostics::diagnostic!("warn"; "isocastd: obs-websocket stays off: {problem}");
            inner.last_error = Some(problem);
            return;
        }
    };
    let Ok(ip) = inner.settings.bind.parse::<IpAddr>() else {
        return; // problem() above refused it.
    };
    let config = Config {
        addr: SocketAddr::new(ip, inner.settings.port),
        password,
        isocast_version: env!("CARGO_PKG_VERSION").into(),
    };
    let engine: Arc<dyn Engine> = Arc::new(Bridge(Arc::downgrade(daemon)));
    match Server::start(config, engine) {
        Ok(server) => {
            isocast_diagnostics::diagnostic!(
                "isocastd: obs-websocket listening on {} ({})",
                inner.settings.url(),
                if server.authentication() {
                    "password"
                } else {
                    "no password"
                }
            );
            if let Some(warning) = inner.settings.bind_warning() {
                isocast_diagnostics::diagnostic!("warn"; "isocastd: {warning}");
            }
            inner.server = Some(server);
        }
        Err(err) => {
            let problem = if err.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "port {} is in use (is OBS running with its WebSocket server on?); give \
                     another with `isocast websocket enable --port N`",
                    inner.settings.port
                )
            } else {
                format!("could not listen on {}: {err}", inner.settings.url())
            };
            isocast_diagnostics::diagnostic!("warn"; "isocastd: obs-websocket stays off: {problem}");
            inner.last_error = Some(problem);
        }
    }
}

fn status(inner: &Inner, generated_password: Option<String>) -> WebsocketStatus {
    let settings = &inner.settings;
    WebsocketStatus {
        enabled: settings.enabled,
        running: inner.server.is_some(),
        port: settings.port,
        bind: settings.bind.clone(),
        url: settings.url(),
        authentication: settings.authentication,
        clients: inner.server.as_ref().map_or(0, Server::clients),
        obs_websocket_version: OBS_WEBSOCKET_VERSION.into(),
        rpc_version: RPC_VERSION,
        last_error: inner.last_error.clone(),
        generated_password,
        warning: None,
        bind_warning: settings.bind_warning(),
    }
}

/// isocastd as the server's [`Engine`]: the method table, and watchers.
struct Bridge(Weak<Daemon>);

impl Engine for Bridge {
    fn call(&self, method: Method, peer: SocketAddr) -> Result<serde_json::Value, RpcError> {
        let Some(daemon) = self.0.upgrade() else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "isocastd is stopping".into(),
            });
        };
        // A network caller (decision 14 of docs/remote-engines.md): held
        // to the local-only policy, and named in events and the audit.
        let caller = Caller {
            origin: isocast_protocol::caller::Origin::ObsWebsocket { addr: peer },
        };
        let request = Request::new(0, method);
        let line = serde_json::to_string(&request).unwrap_or_default();
        let mut audit = diagnostics::ApiCall::new(&line, &caller.peer().unwrap_or_default());
        let method = callers::method_name(&request.method);
        let response = match callers::refusal(&caller, &request.method) {
            Some(error) => Response::err(Some(request.id), error),
            None => {
                let _entered = callers::enter(caller.clone());
                daemon.call(request)
            }
        };
        if let Some(error) = &response.error {
            daemon.note_refusal(&caller, &method, error);
        }
        audit.response(&response);
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(serde_json::Value::Null),
        }
    }

    fn log(&self, line: &str) {
        isocast_diagnostics::diagnostic!("{line}");
    }

    fn watch(&self, kinds: EventKinds) -> Watching {
        let (tx, rx) = mpsc::sync_channel(WATCH_QUEUE_DEPTH);
        let Some(daemon) = self.0.upgrade() else {
            // A closed channel: the session sees the engine went away.
            return Watching {
                events: rx,
                guard: Box::new(()),
            };
        };
        let key = next_watcher_key();
        daemon.watchers.lock().push(Watcher {
            key,
            wanted: kinds,
            tx,
        });
        struct Leave(Weak<Daemon>, u64);
        impl Drop for Leave {
            fn drop(&mut self) {
                if let Some(daemon) = self.0.upgrade() {
                    daemon
                        .watchers
                        .lock()
                        .retain(|watcher| watcher.key != self.1);
                }
            }
        }
        Watching {
            events: rx,
            guard: Box::new(Leave(self.0.clone(), key)),
        }
    }
}
