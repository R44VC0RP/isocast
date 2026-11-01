//! Global hotkeys through the desktop's GlobalShortcuts portal. The desktop
//! picks the keys; each binding's chord is only the preferred trigger.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use isocast_engine::hotkeys::{Edge, Key, OnKey};
use isocast_protocol::hotkeys::{Chord, Phase, Registration};

/// The portal refuses an app id without a desktop entry
/// (packaging/linux/dev.isocast.Isocast.desktop).
const APP_ID: &str = "dev.isocast.Isocast";
/// Connecting, registering and opening the session.
const ANSWER: Duration = Duration::from_secs(5);
/// Binding, which may wait on the desktop's dialog.
const ASSIGN: Duration = Duration::from_secs(30);
const CLOSE: Duration = Duration::from_secs(2);

pub const WAITING: &str =
    "waiting for the desktop's GlobalShortcuts portal; focused input works meanwhile";

pub type OnRegistered = Box<dyn FnOnce(Vec<Registration>) + Send>;

/// Dropping it stops the listener, which drops the session's connection, so
/// the portal ends the session even if it was never closed.
struct Running {
    session: Session<GlobalShortcuts>,
    listener: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct State {
    /// A registration keeps its session only if no later `replace` came.
    turn: u64,
    running: Option<Running>,
    task: Option<tokio::task::JoinHandle<()>>,
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
                Some((_, trigger)) if trigger.is_empty() => Some(format!(
                    "registered with the desktop, which has no keys for it yet; assign them \
                     in its shortcut settings (Hyprland: bind = MODS, KEY, global, \
                     {APP_ID}:{})",
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
        if let Some(task) = state.task.take() {
            task.abort();
        }
        let previous = state.running.take();
        let shared = self.state.clone();
        state.task = Some(runtime.spawn(async move {
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
            let result = match register(shortcuts, generation, on_key).await {
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
                Err(error) => unavailable(&keys, &error),
            };
            done(result);
        }));
    }
}

async fn register(
    shortcuts: Vec<NewShortcut>,
    generation: u64,
    on_key: OnKey,
) -> Result<(Running, Vec<(String, String)>)> {
    let (portal, session, mut edges) = tokio::time::timeout(ANSWER, async {
        // A dedicated connection: Register must come before its first portal
        // call, and ASHPD's process-global one would outlive this session.
        let connection = zbus::Connection::session().await?;
        ashpd::register_host_app_with_connection(
            connection.clone(),
            ashpd::AppID::try_from(APP_ID)?,
        )
        .await
        .with_context(|| {
            format!(
                "register app id {APP_ID}: install its desktop entry with \
                 packaging/linux/install.sh, or scripts/linux-dev-install.sh for a \
                 development build"
            )
        })?;
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
    })
    .await
    .map_err(|_| anyhow::anyhow!("it did not answer within {} s", ANSWER.as_secs()))??;
    let bound = tokio::time::timeout(
        ASSIGN,
        portal.bind_shortcuts(&session, &shortcuts, None, Default::default()),
    )
    .await
    .map_err(|_| anyhow::anyhow!("binding got no answer within {} s", ASSIGN.as_secs()));
    let bound = match bound.and_then(|request| Ok(request?.response()?)) {
        Ok(bound) => bound,
        Err(error) => {
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
                "registered with the desktop, which has no keys for it yet; assign them in its \
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
}
