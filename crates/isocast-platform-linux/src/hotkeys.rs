//! Global hotkeys through the desktop's GlobalShortcuts portal. The desktop
//! picks the keys; each binding's chord is only the preferred trigger.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use ashpd::desktop::{ResponseError, Session};
use futures_util::StreamExt;
use isocast_engine::hotkeys::{Edge, Key, OnKey};
use isocast_protocol::hotkeys::{Chord, Phase, Registration};
use tokio::sync::oneshot;

/// The portal refuses an app id without a desktop entry
/// (packaging/linux/dev.isocast.Isocast.desktop).
const APP_ID: &str = "dev.isocast.Isocast";
/// Connecting, registering and opening the session. Binding has no limit:
/// it waits on the person in the desktop's dialog, and a later `replace`
/// cancels it.
const ANSWER: Duration = Duration::from_secs(5);
const CLOSE: Duration = Duration::from_secs(2);
const REGISTRY: &str = "org.freedesktop.host.portal.Registry";

pub const WAITING: &str =
    "waiting for the desktop's GlobalShortcuts portal; focused input works meanwhile";
const DECLINED: &str = "you declined the desktop's shortcut dialog; change a hotkey or restart \
                        Isocast to be asked again, or use focused input";
const ENDED: &str = "the desktop's shortcut dialog ended without binding them (GNOME's Cancel \
                     ends it this way, as does a desktop error); change a hotkey or restart \
                     Isocast to be asked again, or use focused input";

/// The desktop's shortcut dialog ended without binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialog {
    /// Response 1: cancelled.
    Declined,
    /// Response 2: "ended in some other way". xdg-desktop-portal-gnome 50
    /// answers the person's Cancel with it.
    Ended,
}

impl Dialog {
    fn warning(self) -> &'static str {
        match self {
            Dialog::Declined => DECLINED,
            Dialog::Ended => ENDED,
        }
    }
}

impl std::fmt::Display for Dialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.warning())
    }
}

impl std::error::Error for Dialog {}

/// A later `replace` cancelled the registration.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("replaced by a later registration")
    }
}

impl std::error::Error for Cancelled {}

pub type OnRegistered = Box<dyn FnOnce(Vec<Registration>) + Send>;

/// Dropping it stops the listener, which drops the session's connection, so
/// the portal ends the session even if it was never closed.
struct Running {
    session: Session<GlobalShortcuts>,
    listener: tokio::task::JoinHandle<()>,
}

/// A registration in flight. Dropping `cancel` asks it to stop and close
/// the session it opened.
struct Task {
    handle: tokio::task::JoinHandle<()>,
    cancel: oneshot::Sender<()>,
}

impl Task {
    /// Waits until the portal has closed its session, so that session can't
    /// end after the next one opens.
    async fn stop(self) {
        drop(self.cancel);
        let mut handle = self.handle;
        if tokio::time::timeout(CLOSE * 2, &mut handle).await.is_err() {
            handle.abort();
        }
    }
}

#[derive(Default)]
struct State {
    /// A registration keeps its session only if no later `replace` came.
    turn: u64,
    running: Option<Running>,
    task: Option<Task>,
}

#[derive(Default)]
pub struct LinuxHotkeys {
    state: Arc<Mutex<State>>,
}

impl LinuxHotkeys {
    pub fn shared() -> &'static LinuxHotkeys {
        static SHARED: OnceLock<LinuxHotkeys> = OnceLock::new();
        SHARED.get_or_init(LinuxHotkeys::default)
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

fn runtime() -> Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("linux-hotkeys")
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("hotkey runtime: {error}"))
}

fn focused(keys: &[Key], warning: &str) -> Vec<Registration> {
    keys.iter()
        .map(|key| Registration {
            binding: key.binding.clone(),
            global: false,
            warning: Some(warning.to_string()),
        })
        .collect()
}

pub fn waiting(keys: &[Key]) -> Vec<Registration> {
    focused(keys, WAITING)
}

fn unavailable(keys: &[Key], error: &anyhow::Error) -> Vec<Registration> {
    focused(
        keys,
        &format!(
            "the desktop's GlobalShortcuts portal is unavailable ({error:#}); use focused input"
        ),
    )
}

/// `assigned`: each bound shortcut id with the desktop's trigger description.
fn registrations(keys: Vec<Key>, assigned: &[(String, String)]) -> Vec<Registration> {
    keys.into_iter()
        .map(|key| {
            let trigger = assigned.iter().find(|(id, _)| *id == key.binding);
            let warning = match trigger {
                None => Some(
                    "the desktop's GlobalShortcuts portal did not bind it; use focused input"
                        .to_string(),
                ),
                // xdph always answers with an empty description, even when
                // hyprland.conf binds the shortcut.
                Some((_, trigger)) if trigger.is_empty() => Some(format!(
                    "the desktop doesn't say which keys it uses; if none, assign them in its \
                     shortcut settings (Hyprland: bind = MODS, KEY, global, {APP_ID}:{})",
                    key.binding
                )),
                Some(_) => None,
            };
            Registration {
                global: trigger.is_some(),
                binding: key.binding,
                warning,
            }
        })
        .collect()
}

/// A chord as the shortcuts specification writes it: `CTRL+SHIFT+F13`.
fn trigger(chord: &Chord) -> String {
    let mut parts = Vec::new();
    for (bit, name) in [(1, "CTRL"), (2, "ALT"), (4, "SHIFT"), (8, "LOGO")] {
        if chord.modifiers & bit != 0 {
            parts.push(name.to_string());
        }
    }
    parts.push(keysym(&chord.key));
    parts.join("+")
}

fn keysym(key: &str) -> String {
    let name = match key {
        "space" => "space",
        "return" => "Return",
        "tab" => "Tab",
        "escape" => "Escape",
        "backspace" => "BackSpace",
        "delete" => "Delete",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Prior",
        "pagedown" => "Next",
        "minus" => "minus",
        "equal" => "equal",
        "left_bracket" => "bracketleft",
        "right_bracket" => "bracketright",
        "backslash" => "backslash",
        "semicolon" => "semicolon",
        "quote" => "apostrophe",
        "comma" => "comma",
        "period" => "period",
        "slash" => "slash",
        "grave" => "grave",
        "numpad_decimal" => "KP_Decimal",
        "numpad_multiply" => "KP_Multiply",
        "numpad_plus" => "KP_Add",
        "numpad_clear" => "Clear",
        "numpad_divide" => "KP_Divide",
        "numpad_enter" => "KP_Enter",
        "numpad_minus" => "KP_Subtract",
        "numpad_equal" => "KP_Equal",
        "media_play_pause" => "XF86AudioPlay",
        "media_next" => "XF86AudioNext",
        "media_previous" => "XF86AudioPrev",
        "media_volume_up" => "XF86AudioRaiseVolume",
        "media_volume_down" => "XF86AudioLowerVolume",
        "media_mute" => "XF86AudioMute",
        _ => {
            if let Some(digit) = key.strip_prefix("numpad_") {
                return format!("KP_{digit}");
            }
            if let Some(n) = key.strip_prefix('f').filter(|n| !n.is_empty()) {
                return format!("F{n}");
            }
            key
        }
    };
    name.to_string()
}

impl Running {
    async fn close(self) {
        self.listener.abort();
        let _ = tokio::time::timeout(CLOSE, self.session.close()).await;
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

impl LinuxHotkeys {
    /// Returns at once; binding can wait on a desktop dialog. `done` gets
    /// the registrations unless a later call replaced this one.
    pub fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey, done: OnRegistered) {
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(error) => return done(unavailable(&keys, &error)),
        };
        let mut state = lock(&self.state);
        state.turn += 1;
        let turn = state.turn;
        let before = state.task.take();
        let previous = state.running.take();
        let shared = self.state.clone();
        let (cancel, mut cancelled) = oneshot::channel();
        let handle = runtime.spawn(async move {
            if let Some(before) = before {
                before.stop().await;
            }
            if let Some(previous) = previous {
                previous.close().await;
            }
            if keys.is_empty() {
                return done(Vec::new());
            }
            let shortcuts: Vec<NewShortcut> = keys
                .iter()
                .map(|key| {
                    NewShortcut::new(key.binding.clone(), format!("Isocast: {}", key.binding))
                        .preferred_trigger(trigger(&key.chord).as_str())
                })
                .collect();
            let result = match register(shortcuts, generation, on_key, &mut cancelled).await {
                Ok((running, assigned)) => {
                    let replaced = {
                        let mut state = lock(&shared);
                        if state.turn == turn {
                            state.running = Some(running);
                            None
                        } else {
                            Some(running)
                        }
                    };
                    if let Some(running) = replaced {
                        running.close().await;
                        return;
                    }
                    registrations(keys, &assigned)
                }
                Err(error) if error.is::<Cancelled>() => return,
                Err(error) => match error.downcast_ref::<Dialog>() {
                    Some(dialog) => focused(&keys, dialog.warning()),
                    None => unavailable(&keys, &error),
                },
            };
            done(result);
        });
        state.task = Some(Task { handle, cancel });
    }
}

/// Portals before xdg-desktop-portal 1.19 have no host app Registry.
fn registry_missing(error: &ashpd::Error) -> bool {
    const MISSING: [&str; 2] = [
        "org.freedesktop.DBus.Error.UnknownMethod",
        "org.freedesktop.DBus.Error.UnknownInterface",
    ];
    match error {
        ashpd::Error::PortalNotFound(interface) => interface.as_str() == REGISTRY,
        ashpd::Error::Zbus(zbus::Error::MethodError(name, _, _)) => {
            MISSING.contains(&name.as_str())
        }
        ashpd::Error::Zbus(zbus::Error::FDO(error)) => matches!(
            **error,
            zbus::fdo::Error::UnknownMethod(_) | zbus::fdo::Error::UnknownInterface(_)
        ),
        _ => false,
    }
}

/// Explains a later failure on a portal that couldn't register the app id.
fn unregistered(error: anyhow::Error, registered: bool) -> anyhow::Error {
    if registered
        || matches!(
            error.downcast_ref::<ashpd::Error>(),
            Some(ashpd::Error::PortalNotFound(_))
        )
    {
        error
    } else {
        error.context(format!(
            "this xdg-desktop-portal has no {REGISTRY} (added in 1.19), so it couldn't \
             register app id {APP_ID}"
        ))
    }
}

async fn register(
    shortcuts: Vec<NewShortcut>,
    generation: u64,
    on_key: OnKey,
    cancelled: &mut oneshot::Receiver<()>,
) -> Result<(Running, Vec<(String, String)>)> {
    let open = tokio::time::timeout(ANSWER, async {
        // A dedicated connection: Register must come before its first portal
        // call, and ASHPD's process-global one would outlive this session.
        let connection = zbus::Connection::session().await?;
        let registered = match ashpd::register_host_app_with_connection(
            connection.clone(),
            ashpd::AppID::try_from(APP_ID)?,
        )
        .await
        {
            Ok(()) => true,
            // The portal then takes the app id from the process, if it can.
            Err(error) if registry_missing(&error) => false,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "register app id {APP_ID}: install its desktop entry with \
                     packaging/linux/install.sh, or scripts/linux-dev-install.sh for a \
                     development build"
                )));
            }
        };
        let opened = async {
            let portal = GlobalShortcuts::with_connection(connection).await?;
            let session = portal.create_session(Default::default()).await?;
            let pressed = portal.receive_activated().await?.map(|event| {
                (
                    event.session_handle().to_string(),
                    event.shortcut_id().to_string(),
                    Phase::Press,
                )
            });
            let released = portal.receive_deactivated().await?.map(|event| {
                (
                    event.session_handle().to_string(),
                    event.shortcut_id().to_string(),
                    Phase::Release,
                )
            });
            anyhow::Ok((
                portal,
                session,
                futures_util::stream::select(pressed, released),
            ))
        }
        .await
        .map_err(|error| unregistered(error, registered))?;
        anyhow::Ok((opened, registered))
    });
    // Cancelled before a session handle came back: dropping the connection
    // ends any session the portal opened.
    let opened = tokio::select! {
        opened = open => opened,
        _ = &mut *cancelled => return Err(Cancelled.into()),
    };
    let ((portal, session, mut edges), registered) = opened
        .map_err(|_| anyhow::anyhow!("it did not answer within {} s", ANSWER.as_secs()))??;
    let bound = tokio::select! {
        bound = portal.bind_shortcuts(&session, &shortcuts, None, Default::default()) => {
            bound.and_then(|request| request.response()).map_err(|error| match error {
                ashpd::Error::Response(ResponseError::Cancelled) => Dialog::Declined.into(),
                ashpd::Error::Response(ResponseError::Other) => Dialog::Ended.into(),
                error => unregistered(error.into(), registered),
            })
        }
        _ = &mut *cancelled => Err(Cancelled.into()),
    };
    let bound = match bound {
        Ok(bound) => bound,
        Err(error) => {
            // The specification says this ends the desktop's dialog;
            // xdg-desktop-portal-gnome 50 leaves it open, and drops its answer.
            let _ = tokio::time::timeout(CLOSE, session.close()).await;
            return Err(error);
        }
    };
    let assigned = bound
        .shortcuts()
        .iter()
        .map(|s| (s.id().to_string(), s.trigger_description().to_string()))
        .collect();
    // Session serializes as its object path (ASHPD keeps `path` private).
    let path = serde_json::to_value(&session)?
        .as_str()
        .context("portal session path")?
        .to_string();
    let listener = tokio::spawn(async move {
        // Keeps the connection's portal proxy alive with its subscriptions.
        let _portal = portal;
        while let Some((handle, binding, phase)) = edges.next().await {
            if handle == path {
                on_key(Edge {
                    binding,
                    generation,
                    phase,
                });
            }
        }
    });
    Ok((Running { session, listener }, assigned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_use_the_shortcuts_specification_names() {
        let chord = |key: &str, modifiers| Chord {
            key: key.into(),
            modifiers,
        };
        assert_eq!(trigger(&chord("r", 1 | 4)), "CTRL+SHIFT+r");
        assert_eq!(trigger(&chord("f5", 2 | 8)), "ALT+LOGO+F5");
        assert_eq!(trigger(&chord("f13", 0)), "F13");
        assert_eq!(trigger(&chord("numpad_7", 0)), "KP_7");
        assert_eq!(trigger(&chord("pagedown", 1)), "CTRL+Next");
        assert_eq!(trigger(&chord("media_play_pause", 0)), "XF86AudioPlay");
    }

    fn key(binding: &str) -> Key {
        Key {
            binding: binding.into(),
            chord: Chord {
                key: "f13".into(),
                modifiers: 0,
            },
            hold: false,
        }
    }

    #[test]
    fn bound_shortcuts_are_global_and_the_rest_say_why_not() {
        let keys = vec![key("bound"), key("no_keys"), key("refused")];
        let assigned = vec![
            ("bound".to_string(), "Ctrl+F13".to_string()),
            ("no_keys".to_string(), String::new()),
        ];
        let got = registrations(keys, &assigned);
        assert_eq!(
            got[0],
            Registration {
                binding: "bound".into(),
                global: true,
                warning: None,
            }
        );
        assert!(got[1].global);
        assert_eq!(
            got[1].warning.as_deref(),
            Some(
                "the desktop doesn't say which keys it uses; if none, assign them in its \
                 shortcut settings (Hyprland: bind = MODS, KEY, global, \
                 dev.isocast.Isocast:no_keys)"
            )
        );
        assert!(!got[2].global);
        assert_eq!(
            got[2].warning.as_deref(),
            Some("the desktop's GlobalShortcuts portal did not bind it; use focused input")
        );
    }

    #[test]
    fn a_failed_portal_leaves_every_binding_focused() {
        let keys = vec![key("a"), key("b")];
        let error = anyhow::anyhow!("it did not answer within 5 s");
        let got = unavailable(&keys, &error);
        assert_eq!(got.len(), 2);
        for registration in &got {
            assert!(!registration.global);
            assert_eq!(
                registration.warning.as_deref(),
                Some(
                    "the desktop's GlobalShortcuts portal is unavailable (it did not answer \
                     within 5 s); use focused input"
                )
            );
        }
        assert!(
            waiting(&keys)
                .iter()
                .all(|r| !r.global && r.warning.as_deref() == Some(WAITING))
        );
    }

    #[test]
    fn a_portal_without_the_registry_is_told_apart_from_other_failures() {
        let missing = ashpd::Error::PortalNotFound(REGISTRY.try_into().unwrap());
        assert!(registry_missing(&missing));
        let other_portal = ashpd::Error::PortalNotFound(
            "org.freedesktop.portal.GlobalShortcuts".try_into().unwrap(),
        );
        assert!(!registry_missing(&other_portal));
        let missing_message = other_portal.to_string();
        assert_eq!(
            format!("{:#}", unregistered(other_portal.into(), false)),
            missing_message
        );
        let unknown = ashpd::Error::from(zbus::fdo::Error::UnknownMethod("Register".into()));
        assert!(registry_missing(&unknown));
        let refused = ashpd::Error::from(zbus::fdo::Error::AccessDenied("no entry".into()));
        assert!(!registry_missing(&refused));
    }

    #[test]
    fn a_failure_after_skipping_the_registry_says_the_portal_is_old() {
        let error = || anyhow::anyhow!("CreateSession refused");
        assert_eq!(
            format!("{:#}", unregistered(error(), true)),
            "CreateSession refused"
        );
        assert_eq!(
            format!("{:#}", unregistered(error(), false)),
            "this xdg-desktop-portal has no org.freedesktop.host.portal.Registry (added in \
             1.19), so it couldn't register app id dev.isocast.Isocast: CreateSession refused"
        );
    }

    #[test]
    fn a_dialog_that_ends_without_binding_has_its_own_message() {
        assert_eq!(
            Dialog::Declined.warning(),
            "you declined the desktop's shortcut dialog; change a hotkey or restart Isocast \
             to be asked again, or use focused input"
        );
        assert_eq!(
            Dialog::Ended.warning(),
            "the desktop's shortcut dialog ended without binding them (GNOME's Cancel ends it \
             this way, as does a desktop error); change a hotkey or restart Isocast to be \
             asked again, or use focused input"
        );
        let error = anyhow::Error::from(Dialog::Ended);
        assert_eq!(error.downcast_ref::<Dialog>(), Some(&Dialog::Ended));
    }
}
