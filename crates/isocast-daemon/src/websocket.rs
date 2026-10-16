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

/// The password clients must give, if any; an error if one should exist
/// but can't be read (the server then stays off).
fn password(settings: &WebsocketSettings) -> Result<Option<String>, String> {
    if !settings.authentication {
        return Ok(None);
    }
    if let Some(password) = std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty()) {
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
        let mut inner = self.inner.lock().unwrap();
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
        status(&self.inner.lock().unwrap(), None)
    }

    pub(super) fn enable(
        &self,
        daemon: &Arc<Daemon>,
        params: WebsocketEnableParams,
    ) -> Result<WebsocketStatus, RpcError> {
        let mut inner = self.inner.lock().unwrap();
        let mut settings = inner.settings.clone();
        settings.enabled = true;
        if let Some(port) = params.port {
            settings.port = port;
        }
        if let Some(bind) = params.bind {
            settings.bind = bind;
        }
        // Every outcome below but no_password ends with a password.
        let after = WebsocketSettings {
            authentication: !params.no_password,
            ..settings.clone()
        };
        if let Some(problem) = after.problem() {
            return Err(invalid(problem));
        }
        if params.no_password && params.password.is_some() {
            return Err(invalid(
                "password: give a password or no_password, not both",
            ));
        }
        let mut generated = None;
        if params.no_password {
            settings.authentication = false;
            // The stored password goes too; enabling again makes a new one.
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
        } else if !settings.authentication || password(&settings).is_err() {
            // On for the first time, or turned back on after --no-password,
            // without a password: make one and show it once.
            let new = isocast_obsws::generate_password().map_err(|err| failed(err.into()))?;
            set_secret(&service(), &account(), &new).map_err(failed)?;
            isocast_diagnostics::redact::remember_secret(&service(), &account(), &new);
            settings.authentication = true;
            generated = Some(new);
        }
        save(&settings)?;
        inner.settings = settings;
        restart(&mut inner, daemon);
        Ok(status(&inner, generated))
    }

    pub(super) fn disable(&self) -> Result<WebsocketStatus, RpcError> {
        let mut inner = self.inner.lock().unwrap();
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
        let server = self.inner.lock().unwrap().server.take();
        drop(server);
    }
}

fn save(settings: &WebsocketSettings) -> Result<(), RpcError> {
    let path = settings_path();
    let partial = path.with_extension("json.partial");
    let bytes = serde_json::to_vec_pretty(settings).map_err(|err| failed(err.into()))?;
    std::fs::write(&partial, bytes)
        .and_then(|()| std::fs::rename(&partial, &path))
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
    }
}

/// isocastd as the server's [`Engine`]: the method table, and watchers.
struct Bridge(Weak<Daemon>);

impl Engine for Bridge {
    fn call(&self, method: Method, peer: &str) -> Result<serde_json::Value, RpcError> {
        let Some(daemon) = self.0.upgrade() else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "isocastd is stopping".into(),
            });
        };
        let request = Request::new(0, method);
        let line = serde_json::to_string(&request).unwrap_or_default();
        let mut audit = diagnostics::ApiCall::new(
            &line,
            &serde_json::json!({ "transport": "obs-websocket", "peer": peer }),
        );
        let response = daemon.call(request);
        audit.response(&response);
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(serde_json::Value::Null),
        }
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
        daemon.watchers.lock().unwrap().push(Watcher {
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
                        .unwrap_or_else(|p| p.into_inner())
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
