//! Global hotkeys through the desktop's GlobalShortcuts portal. Each binding
//! is a shortcut with its chord as the preferred trigger; the desktop picks
//! the keys (GNOME and KDE ask the person, Hyprland takes them from its own
//! config: `bind = MODS, KEY, global, dev.isocast.Isocast:<binding>`) and
//! reports presses and releases. A missing or silent portal leaves every
//! binding to focused input.

use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use isocast_engine::hotkeys::{Edge, GlobalHotkeys, Key, OnKey};
use isocast_protocol::hotkeys::{Chord, Phase, Registration};

/// The app id of packaging/linux/dev.isocast.Isocast.desktop. Host apps
/// must register one before the portal grants GlobalShortcuts.
const APP_ID: &str = "dev.isocast.Isocast";
/// Bounds connecting, registering and opening the session: the portal
/// answers these without asking anyone.
const ANSWER: Duration = Duration::from_secs(5);
/// How long binding may wait for the desktop's shortcut dialog.
const ASSIGN: Duration = Duration::from_secs(30);
const CLOSE: Duration = Duration::from_secs(2);

/// The portal session of the current registrations; replaced as a whole.
struct Running {
    session: Session<GlobalShortcuts>,
    listener: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct LinuxHotkeys {
    running: Mutex<Option<Running>>,
}

impl LinuxHotkeys {
    /// The process's hotkeys (one portal session at a time).
    pub fn shared() -> &'static LinuxHotkeys {
        static SHARED: OnceLock<LinuxHotkeys> = OnceLock::new();
        SHARED.get_or_init(LinuxHotkeys::default)
    }
}

/// One runtime for every session: its listener runs between calls. Each
/// session opens a connection of its own on it.
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

/// Run `task` on the hotkey runtime and wait at most `limit` for it. The
/// caller is never a runtime thread, and never waits on a wedged one.
fn run<T: Send + 'static>(
    limit: Duration,
    task: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let (send, receive) = mpsc::sync_channel(1);
    runtime()?.spawn(async move {
        let _ = send.send(task.await);
    });
    receive
        .recv_timeout(limit + Duration::from_secs(1))
        .context("the hotkey runtime did not answer")?
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

/// The XKB keysym name for one of `hotkeys::key_names`.
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
    fn close(self) {
        self.listener.abort();
        let session = self.session;
        let _ = run(CLOSE, async move {
            let _ = tokio::time::timeout(CLOSE, session.close()).await;
            Ok(())
        });
    }
}

impl GlobalHotkeys for LinuxHotkeys {
    fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = running.take() {
            previous.close();
        }
        if keys.is_empty() {
            return Vec::new();
        }
        let shortcuts: Vec<NewShortcut> = keys
            .iter()
            .map(|key| {
                NewShortcut::new(key.binding.clone(), format!("Isocast: {}", key.binding))
                    .preferred_trigger(trigger(&key.chord).as_str())
            })
            .collect();
        let bound = run(ANSWER + ASSIGN, register(shortcuts, generation, on_key));
        let (started, assigned) = match bound {
            Ok(bound) => bound,
            Err(error) => {
                let warning = format!(
                    "the desktop's GlobalShortcuts portal is unavailable ({error:#}); use focused \
                     input"
                );
                return keys
                    .into_iter()
                    .map(|key| Registration {
                        binding: key.binding,
                        global: false,
                        warning: Some(warning.clone()),
                    })
                    .collect();
            }
        };
        *running = Some(started);
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
}

/// Open a session on a connection of its own, bind `shortcuts`, and deliver
/// this session's presses and releases. Returns the session and the
/// trigger the desktop describes for each binding it bound.
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
}
