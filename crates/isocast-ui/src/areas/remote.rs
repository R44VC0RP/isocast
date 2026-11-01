//! Area J: remote control (docs/remote-engines.md, R2; ui/areas/remote/).
//!
//! Isocast opens standalone, like OBS: nothing about remote control shows
//! until Settings → Remote turns on one of its two switches (both off):
//!
//! - **Control other computers** is this app's own choice (kept beside the
//!   layout, in `isocast-ui-remote.json`). It adds the engine picker in the
//!   status bar (this machine, the engines this app is paired with, those
//!   found on the network through the OS's DNS-SD, Add Engine…), pairing
//!   with an engine (its address, the code it shows, then Open) and the
//!   paired engines' list (Forget). Choosing an engine opens it in a window
//!   of its own ([`seam::open_engine_window`]).
//! - **Let other computers control this one** is the engine's
//!   (`remote_enable`/`remote_disable`): its name and identity, Pair a
//!   Computer (`remote_pair_begin`: the code and its countdown), the paired
//!   computers (`remote_clients`: connected now or last seen, Revoke) and
//!   the folders they can use (`folder_roots`, `folder_roots_set`).
//!
//! Remote settings change only on the machine itself (decision 4), so a
//! window on another computer's engine hides them and says where to change
//! them. It shows who else is connected to that engine (presence), toasts
//! who started or stopped a recording or stream or changed the setup (the
//! events' `by`), and its file pickers browse the engine's disk
//! (`files_roots`, `files_list`; [`choose`]); "Show in Finder" becomes
//! "Show on <engine>" ([`reveal`]).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_client::Engine;
use isocast_protocol::api::{Event, Method};
use isocast_protocol::hello::Hello;
use isocast_protocol::remote::{RemotePairWindow, RemotePeer, RemoteRevokeParams, RemoteStatus};
use isocast_protocol::roots::{
    FilesList, FilesListParams, FilesRoots, FolderRoots, FolderRootsSetParams,
};
use isocast_protocol::stream::StreamState;
use serde_json::{Value, json};
use slint::ComponentHandle;

use super::{Area, Changed};
use crate::app::App;
use crate::kit::{MenuBuilder, model};
use crate::platform::Choose;
use crate::{
    AppWindow, NavTab, RemoteArea, RemoteFileRow, RemoteFolderRow, RemotePeerRow, ToastKind,
};

/// Snapshot states (`--state NAME[:ARG]`): sample data, nothing changed on
/// the engine or the network.
pub const STATES: &[&str] = &[
    "remote-sample",
    "remote-window:NAME",
    "remote-pairing[:STEP]",
    "remote-browser",
];

/// The window's engine and windows: R2 core's [`crate::engine`] handle
/// (one window per engine, `--engine` in a new process).
pub mod seam {
    use isocast_client::Engine;

    /// The engine this window controls.
    pub fn window_engine() -> Engine {
        crate::engine::current()
    }

    /// Open `engine` in a window of its own.
    pub fn open_engine_window(engine: Engine) -> anyhow::Result<()> {
        crate::engine::open_window(&engine)
    }
}

/// What a file browser calls with the chosen paths (none if cancelled).
type Chosen = Box<dyn FnOnce(&Rc<App>, Option<Vec<String>>)>;

/// The app's own remote choice, kept beside the layout.
fn prefs_path() -> PathBuf {
    crate::prefs::path().with_file_name("isocast-ui-remote.json")
}

fn read_prefs() -> Value {
    std::fs::read_to_string(prefs_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Change the app's remote choices, keeping the others.
fn write_prefs(change: impl FnOnce(&mut serde_json::Map<String, Value>)) {
    let mut saved = read_prefs();
    if let Some(map) = saved.as_object_mut() {
        change(map);
    }
    let path = prefs_path();
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    let _ = std::fs::write(&path, format!("{saved:#}\n"));
}

fn read_control_others() -> bool {
    read_prefs()
        .get("control_others")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn save_control_others(on: bool) {
    write_prefs(|map| {
        map.insert("control_others".into(), json!(on));
    });
}

/// Whether this window's engine (another computer's) plays its program's
/// sound here: off until chosen, then remembered for that paired engine.
pub fn sound_on() -> bool {
    let Some(key) = sound_key() else {
        return false;
    };
    read_prefs()
        .get("sound")
        .and_then(|sound| sound.get(&key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Remember [`sound_on`] for this window's engine.
pub fn set_sound_on(on: bool) {
    let Some(key) = sound_key() else { return };
    write_prefs(|map| {
        let sound = map.entry("sound").or_insert_with(|| json!({}));
        if let Some(sound) = sound.as_object_mut() {
            sound.insert(key, json!(on));
        }
    });
}

/// The window's engine as remembered: its paired identity, else the
/// address it was opened with.
fn sound_key() -> Option<String> {
    let Engine::Remote { target } = crate::engine::current() else {
        return None;
    };
    let paired = isocast_client::remote::engines().ok().and_then(|peers| {
        peers
            .into_iter()
            .find(|peer| peer.id == target || peer.name == target)
            .map(|peer| peer.id)
    });
    Some(paired.unwrap_or(target))
}

/// Whether `ip` is on a local network: private, link-local, loopback,
/// unique-local or Tailscale's 100.64/10 (decision 2), after unmapping
/// IPv4-mapped IPv6.
fn local_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        ip => ip,
    };
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_private()
                || v4.is_link_local()
                || v4.is_loopback()
                || (a == 100 && (b & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// An engine address's host, without `tls://` or a port.
fn host_of(address: &str) -> String {
    let address = address.trim().trim_start_matches("tls://");
    if let Ok(socket) = address.parse::<std::net::SocketAddr>() {
        return socket.ip().to_string();
    }
    match address.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() && !host.contains(':') => host.into(),
        _ => address.trim_matches(['[', ']']).into(),
    }
}

/// Whether every address `address` names is on a local network (a
/// `.local` name always is). Resolves names: call off the window's thread.
fn address_is_local(address: &str) -> bool {
    let host = host_of(address);
    if host.ends_with(".local") || host.ends_with(".local.") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return local_ip(ip);
    }
    match (host.as_str(), isocast_protocol::remote::DEFAULT_PORT).to_socket_addrs() {
        Ok(addresses) => {
            let addresses: Vec<_> = addresses.collect();
            !addresses.is_empty() && addresses.iter().all(|a| local_ip(a.ip()))
        }
        // Not resolvable: pairing says so.
        Err(_) => true,
    }
}

/// "5 min ago", "3 h ago", "2 days ago".
fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => {
            let days = seconds / 86400;
            format!("{days} day{} ago", if days == 1 { "" } else { "s" })
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An identity for reading aloud: its first 24 hex digits in fours.
fn grouped(id: &str) -> String {
    let short: String = id.chars().take(24).collect();
    let groups: Vec<String> = short
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    format!("{} …", groups.join(" "))
}

/// "This Mac" and "Mac" (or PC), for the machine the app runs on.
fn this_machine() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("This PC", "PC")
    } else if cfg!(target_os = "linux") {
        ("This computer", "computer")
    } else {
        ("This Mac", "Mac")
    }
}

/// Run `work` on a thread of its own, then `done` on the window's thread
/// with what it returned.
fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&Rc<App>, T) + Send + 'static,
) {
    std::thread::spawn(move || {
        let result = work();
        let _ = slint::invoke_from_event_loop(move || {
            crate::window::with_app(|app| done(app, result));
        });
    });
}

/// A pairing in progress on this app (the client side).
#[derive(Default)]
struct Pairing {
    step: String,
    address: String,
    name: String,
    public: bool,
    busy: bool,
    error: String,
    /// The paired engine's id, once paired.
    paired: Option<String>,
}

/// The engine file browser's state.
struct Browser {
    /// No native chooser: list this computer, even in a remote window.
    desktop_fallback: bool,
    roots: Vec<(String, String)>,
    path: String,
    parent: Option<String>,
    entries: Vec<RemoteFileRow>,
    selected: String,
    folders: bool,
    show_only: bool,
    extensions: Vec<String>,
    note: String,
    busy: bool,
    done: Option<Chosen>,
    /// The engine's home folder (from `files_roots`), for "~" in paths.
    home: String,
}

#[derive(Default)]
pub struct Remote {
    // ---- Settings → Remote (this engine's own)
    status: RefCell<Option<RemoteStatus>>,
    clients: RefCell<Vec<RemotePeer>>,
    roots: RefCell<Option<FolderRoots>>,
    /// The pairing code shown, and when it expires.
    code: RefCell<Option<(String, Instant)>>,
    pair_problem: RefCell<String>,
    busy: Cell<bool>,
    control_others: Cell<bool>,
    // ---- This window's engine
    remote_window: Cell<bool>,
    hello: RefCell<Option<Hello>>,
    /// This app's identity (id, name), once read.
    me: RefCell<Option<(String, String)>>,
    // ---- The engines this app knows
    paired: RefCell<Vec<RemotePeer>>,
    found: RefCell<BTreeMap<String, isocast_discovery::Found>>,
    watch: RefCell<Option<isocast_discovery::Watch>>,
    pairing: RefCell<Pairing>,
    browser: RefCell<Option<Browser>>,
    /// When each client's "changed the setup" toast last showed.
    setup_toasts: RefCell<HashMap<String, Instant>>,
    /// Revocation was said once for this link.
    revoked_said: Cell<bool>,
    /// Sample data is showing (snapshots): the engine isn't asked.
    sampled: Cell<bool>,
    ticks: Cell<u32>,
    timer: slint::Timer,
}

impl Area for Remote {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<RemoteArea>();
        let (machine, word) = this_machine();
        area.set_this_machine(machine.into());
        area.set_this_machine_word(word.into());
        let remote = !seam::window_engine().is_local();
        self.remote_window.set(remote);
        area.set_remote_window(remote);
        if let Some(name) = crate::engine::name() {
            area.set_engine_name(name.into());
        }
        if !app.mode.offscreen {
            self.control_others.set(read_control_others());
        }

        macro_rules! on {
            ($setter:ident, | $app:ident $(, $arg:ident)* | $body:expr) => {{
                let weak = Rc::downgrade(app);
                area.$setter(move |$($arg),*| {
                    if let Some($app) = weak.upgrade() {
                        $body;
                    }
                });
            }};
        }
        on!(on_set_control_others, |app, on| app
            .remote
            .set_control_others(&app, on));
        on!(on_set_allow_control, |app, on| app
            .remote
            .set_allow_control(&app, on));
        on!(on_pair_begin, |app| app.remote.pair_begin(&app));
        on!(on_pair_cancel, |app| {
            *app.remote.code.borrow_mut() = None;
            app.remote.show(&app);
        });
        on!(on_revoke, |app, id| app.remote.revoke(&app, &id));
        on!(on_forget, |app, id| app.remote.forget(&app, &id));
        on!(on_add_folder, |app| app.remote.add_folder(&app));
        on!(on_remove_folder, |app, path| app
            .remote
            .remove_folder(&app, &path));
        on!(on_default_folders, |app| app.remote.set_folders(&app, None));
        on!(on_copy_fingerprint, |app| {
            let id = app
                .remote
                .status
                .borrow()
                .as_ref()
                .and_then(|s| s.identity.as_ref())
                .map(|i| i.id.clone());
            if let Some(id) = id {
                crate::platform::copy_text(&id);
                app.toast(ToastKind::Success, "Identity copied", id);
            }
        });
        on!(on_add_engine, |app| app.remote.open_pairing(&app, None));
        on!(on_pairing_next, |app, address| app
            .remote
            .pairing_next(&app, &address));
        on!(on_pairing_code, |app, code| app
            .remote
            .pairing_code(&app, &code));
        on!(on_pairing_open, |app| app.remote.pairing_finish(&app));
        on!(on_pairing_cancel, |app| {
            *app.remote.pairing.borrow_mut() = Pairing::default();
            if let Some(window) = app.window() {
                window.global::<RemoteArea>().set_pairing_shown(false);
            }
        });
        on!(on_browser_go, |app, path| app
            .remote
            .list(&app, &path, None));
        on!(on_browser_up, |app| {
            let parent = app
                .remote
                .browser
                .borrow()
                .as_ref()
                .and_then(|b| b.parent.clone());
            if let Some(parent) = parent {
                app.remote.list(&app, &parent, None);
            }
        });
        on!(on_browser_select, |app, path| {
            if let Some(b) = app.remote.browser.borrow_mut().as_mut() {
                b.selected = path.into();
            }
            app.remote.show_browser(&app);
        });
        on!(on_browser_open_entry, |app, path| app
            .remote
            .open_entry(&app, &path));
        on!(on_browser_choose, |app| app.remote.browser_choose(&app));
        on!(on_browser_cancel, |app| app
            .remote
            .browser_close(&app, None));

        if !app.mode.offscreen {
            // The pairing code's countdown each second; who's connected
            // and the engine's remote status every few.
            let weak = Rc::downgrade(app);
            self.timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(1),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.remote.tick(&app);
                    }
                },
            );
            if self.control_others.get() {
                self.start_discovery();
            }
            self.load_paired();
        }
        self.show(app);
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if !changed.any(Changed::LINK) || app.mode.offscreen || self.sampled.get() {
            return;
        }
        let (link, error) = {
            let s = app.session.borrow();
            (s.link, s.link_error.clone())
        };
        match link {
            Some(crate::daemon::Link::Connected) => {
                self.revoked_said.set(false);
                self.reload(app);
            }
            // A window on another computer's engine that no longer
            // accepts this app: say why once (connection states are the
            // window's own).
            Some(crate::daemon::Link::Failed | crate::daemon::Link::Disconnected)
                if self.remote_window.get()
                    && !self.revoked_said.get()
                    && trust_refused(&error) =>
            {
                self.revoked_said.set(true);
                let name = self.engine_name(app);
                app.toast(
                    ToastKind::Error,
                    &format!("{name} no longer accepts this app"),
                    format!(
                        "Its pairing was revoked or reset on {name}. Forget it in Settings → Remote, then pair again with a code from {name}."
                    ),
                );
            }
            _ => {}
        }
    }

    fn event(&self, app: &Rc<App>, event: &Event) {
        if self.sampled.get() {
            return;
        }
        match event {
            Event::RemoteClientConnected { .. } | Event::RemoteClientDisconnected { .. } => {
                self.load_clients(app);
            }
            Event::RemotePaired { client } => {
                if !self.remote_window.get() {
                    if self.code.borrow_mut().take().is_some() {
                        self.pair_problem.borrow_mut().clear();
                    }
                    app.toast(
                        ToastKind::Success,
                        &format!("Paired with {}", client.name),
                        "It can control this engine now. Revoke it here any time.",
                    );
                }
                self.load_clients(app);
            }
            Event::RemoteRevoked { client_id } => {
                let others = !self.remote_window.get()
                    || self
                        .me
                        .borrow()
                        .as_ref()
                        .is_none_or(|(id, _)| id != client_id);
                if others {
                    self.load_clients(app);
                }
            }
            Event::RemoteDisabled => {
                if !self.remote_window.get() {
                    self.load_status(app);
                }
            }
            Event::Started {
                by: Some(by),
                cause,
                ..
            } if !self.is_me(by) => {
                let what = match cause {
                    Some(isocast_protocol::api::TakeCause::Split) => "split the recording",
                    _ => "started recording",
                };
                self.said(app, by, what);
            }
            Event::Stopped { by: Some(by), .. } if !self.is_me(by) => {
                self.said(app, by, "stopped recording");
            }
            Event::Stream {
                stream,
                by: Some(by),
            } if !self.is_me(by) => match stream.state {
                StreamState::Connecting => self.said(app, by, "started streaming"),
                StreamState::Offline => self.said(app, by, "stopped streaming"),
                _ => {}
            },
            Event::SetupChanged { by: Some(by), .. } if !self.is_me(by) => {
                // At most one toast per client every 20 s: a drag is many
                // changes.
                let fresh = self
                    .setup_toasts
                    .borrow()
                    .get(by)
                    .is_none_or(|at| at.elapsed() > Duration::from_secs(20));
                if fresh {
                    self.setup_toasts
                        .borrow_mut()
                        .insert(by.clone(), Instant::now());
                    self.said(app, by, "changed the setup");
                }
            }
            _ => {}
        }
    }

    fn context_menu(&self, app: &Rc<App>, owner: &str) -> Option<MenuBuilder> {
        (owner == "engine:").then(|| self.picker_menu(app))
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
        if owner != "engine:" {
            return false;
        }
        if id == "engine:add" {
            self.open_pairing(app, None);
        } else if id == "engine:settings" {
            app.open_dialog("settings", "remote");
        } else if id == "engine:local" {
            if self.remote_window.get() {
                self.open_window(app, Engine::Local);
            }
        } else if let Some(engine) = id.strip_prefix("engine:open:") {
            let current =
                matches!(seam::window_engine(), Engine::Remote { target } if target == engine);
            if !current {
                self.open_window(
                    app,
                    Engine::Remote {
                        target: engine.into(),
                    },
                );
            }
        } else if let Some(key) = id.strip_prefix("engine:pair:") {
            let found = self.found.borrow().get(key).cloned();
            if let Some(found) = found {
                self.open_pairing(app, Some(found));
            }
        }
        true
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        let area = window.global::<RemoteArea>();
        match name {
            "remote-sample" => self.sample(app),
            "remote-window" => {
                self.remote_window.set(true);
                area.set_remote_window(true);
                area.set_engine_name(if arg.is_empty() { "exon-desk" } else { arg }.into());
                self.sample(app);
            }
            "remote-pairing" => {
                self.sampled.set(true);
                let mut pairing = self.pairing.borrow_mut();
                *pairing = Pairing {
                    step: if arg.is_empty() { "address" } else { arg }.into(),
                    address: "exon-desk.local".into(),
                    name: "exon-desk".into(),
                    ..Pairing::default()
                };
                match arg {
                    "public" => {
                        pairing.step = "address".into();
                        pairing.address = "203.0.113.20".into();
                        pairing.public = true;
                    }
                    "wrong" => {
                        pairing.step = "code".into();
                        pairing.error = pair_error(
                            "pairing confirmation failed; open a new window locally",
                            "exon-desk",
                        );
                    }
                    _ => {}
                }
                drop(pairing);
                self.show_pairing(app);
            }
            "remote-browser" => {
                self.sampled.set(true);
                let entries = [
                    ("Isocast", true, ""),
                    ("Overlays", true, ""),
                    ("intro.mov", false, "84.2 MB"),
                    ("lower-third.png", false, "212 KB"),
                ];
                *self.browser.borrow_mut() = Some(Browser {
                    desktop_fallback: false,
                    roots: vec![
                        (r"C:\Users\Ryan\Videos".into(), "Videos".into()),
                        (r"C:\Users\Ryan\Pictures".into(), "Pictures".into()),
                        (r"C:\Users\Ryan\Desktop".into(), "Desktop".into()),
                    ],
                    path: r"C:\Users\Ryan\Videos".into(),
                    parent: None,
                    entries: entries
                        .iter()
                        .map(|(name, folder, detail)| RemoteFileRow {
                            name: (*name).into(),
                            path: format!(r"C:\Users\Ryan\Videos\{name}").into(),
                            folder: *folder,
                            detail: (*detail).into(),
                        })
                        .collect(),
                    selected: r"C:\Users\Ryan\Videos\intro.mov".into(),
                    home: r"C:\Users\Ryan".into(),
                    folders: false,
                    show_only: false,
                    extensions: Vec::new(),
                    note: "Media files".into(),
                    busy: false,
                    done: None,
                });
                area.set_engine_name("exon-desk".into());
                area.set_browser_title("Choose a Video".into());
                area.set_browser_prompt("Choose".into());
                self.show_browser(app);
            }
            _ => return None,
        }
        Some(Ok(()))
    }
}

/// Whether a lost link's error says the engine refused this app's key
/// (revoked, or the engine reset its trust), not that it went away.
fn trust_refused(error: &str) -> bool {
    // Only the engine's refusal of this app's key: its TLS alert about our
    // certificate. A timeout, a reset or an unreachable engine is a lost
    // link (the window's Reconnecting), not a revocation.
    let error = error.to_ascii_lowercase().replace(' ', "");
    error.contains("fatalalert")
        && ["certificate", "accessdenied"]
            .iter()
            .any(|word| error.contains(word))
}

/// What to tell the user about a failed pairing with `name`.
fn pair_error(message: &str, name: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("identity changed") {
        format!(
            "{name}'s identity changed since this app paired with it (Isocast was reinstalled or reset there). Forget it in Settings → Remote, then pair again."
        )
    } else if lower.contains("confirmation failed") {
        format!(
            "That code didn't match. Each code allows one try: show a new one on {name} (Settings → Remote → Pair a Computer) and type it here. After 3 wrong tries {name} stops pairing for a minute."
        )
    } else if lower.contains("expired") || lower.contains("timed out") {
        format!("The code expired. Show a new one on {name} and type it here.")
    } else if lower.contains("expected six digits") {
        "Type the 6 digits the engine shows.".into()
    } else if lower.contains("could not connect") || lower.contains("could not be resolved") {
        format!(
            "Couldn't reach {name} for pairing. Check that it shows a code now (codes last 2 minutes, and 3 wrong tries stop pairing for a minute), that \"Let other computers control this one\" is on there, and that both computers are on the same network.\n\n{message}"
        )
    } else {
        message.to_string()
    }
}

impl Remote {
    fn is_me(&self, by: &str) -> bool {
        self.remote_window.get()
            && self
                .me
                .borrow()
                .as_ref()
                .is_some_and(|(_, name)| name == by)
    }

    /// "exon-desk" (the engine's own name once `hello` said it).
    fn engine_name(&self, app: &Rc<App>) -> String {
        app.window()
            .map(|w| w.global::<RemoteArea>().get_engine_name().to_string())
            .unwrap_or_default()
    }

    /// A toast naming who did something on this engine.
    fn said(&self, app: &Rc<App>, by: &str, what: &str) {
        app.toast(
            ToastKind::Info,
            &format!("{by} {what}"),
            "From another computer.",
        );
    }

    fn open_window(&self, app: &Rc<App>, engine: Engine) {
        if let Err(error) = seam::open_engine_window(engine) {
            app.toast(
                ToastKind::Error,
                "Couldn’t open the engine",
                format!("{error:#}"),
            );
        }
    }

    // ---------- Loading ----------

    /// Settings → Remote came up: what it shows, fresh.
    pub fn page_shown(&self, app: &Rc<App>) {
        if self.sampled.get() {
            self.show(app);
            return;
        }
        if !app.mode.offscreen {
            self.load_paired();
        }
        self.load_status(app);
        self.show(app);
    }

    /// Everything again (connected, reconnected).
    fn reload(&self, app: &Rc<App>) {
        if self.remote_window.get() {
            app.call_as::<Hello>(Method::Hello, |app, result| {
                if let Ok(hello) = result {
                    if let Some(window) = app.window() {
                        window
                            .global::<RemoteArea>()
                            .set_engine_name(hello.name.clone().into());
                    }
                    *app.remote.hello.borrow_mut() = Some(hello);
                }
            });
            if self.me.borrow().is_none() {
                background(isocast_client::remote::identity, |app, result| {
                    if let Ok(identity) = result {
                        *app.remote.me.borrow_mut() = Some((identity.id, identity.name));
                        app.remote.show(app);
                    }
                });
            }
            self.load_clients(app);
        } else {
            self.load_status(app);
        }
    }

    fn load_status(&self, app: &Rc<App>) {
        if self.remote_window.get() {
            return;
        }
        app.call_as::<RemoteStatus>(Method::RemoteStatus, |app, result| {
            let remote = &app.remote;
            match result {
                Ok(status) => {
                    let enabled = status.enabled;
                    // A code that was used (or burnt by a wrong try)
                    // before it ran out.
                    let open = status.pairing_open;
                    *remote.status.borrow_mut() = Some(status);
                    if !open && remote.code.borrow().is_some() {
                        *remote.code.borrow_mut() = None;
                        *remote.pair_problem.borrow_mut() =
                            "That code was used: the other computer is paired, or typed it wrong (each code allows one try). Show a new one to pair again.".into();
                    }
                    if enabled {
                        remote.load_clients(app);
                        remote.load_roots(app);
                    }
                }
                // An older engine without remote control: nothing to show.
                Err(error) if error.is_unknown_method() => {}
                Err(_) => {}
            }
            remote.show(app);
        });
    }

    fn load_clients(&self, app: &Rc<App>) {
        let wanted =
            self.remote_window.get() || self.status.borrow().as_ref().is_some_and(|s| s.enabled);
        if !wanted {
            return;
        }
        app.call_as::<Vec<RemotePeer>>(Method::RemoteClients, |app, result| {
            if let Ok(clients) = result {
                *app.remote.clients.borrow_mut() = clients;
                app.remote.show(app);
            }
        });
    }

    fn load_roots(&self, app: &Rc<App>) {
        app.call_as::<FolderRoots>(Method::FolderRoots, |app, result| {
            if let Ok(roots) = result {
                *app.remote.roots.borrow_mut() = Some(roots);
                app.remote.show(app);
            }
        });
    }

    fn load_paired(&self) {
        *self.paired.borrow_mut() = isocast_client::remote::engines().unwrap_or_default();
    }

    fn start_discovery(&self) {
        if self.watch.borrow().is_some() || !isocast_discovery::supported() {
            return;
        }
        let watch = isocast_discovery::watch(Box::new(|event| {
            let _ = slint::invoke_from_event_loop(move || {
                crate::window::with_app(|app| match event {
                    isocast_discovery::Event::Found(found) => {
                        app.remote
                            .found
                            .borrow_mut()
                            .insert(found.name.clone(), found);
                    }
                    isocast_discovery::Event::Lost { name } => {
                        app.remote.found.borrow_mut().remove(&name);
                    }
                    isocast_discovery::Event::Error(_) => {}
                });
            });
        }));
        match watch {
            Ok(watch) => *self.watch.borrow_mut() = Some(watch),
            Err(error) => isocast_diagnostics::diagnostic!("warn"; "engine discovery: {error:#}"),
        }
    }

    fn tick(&self, app: &Rc<App>) {
        if self.sampled.get() {
            return;
        }
        let ticks = self.ticks.get().wrapping_add(1);
        self.ticks.set(ticks);
        let expired = self
            .code
            .borrow()
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until);
        if expired {
            *self.code.borrow_mut() = None;
            *self.pair_problem.borrow_mut() = "The code expired. Show a new one to pair.".into();
        }
        let connected = app.session.borrow().link == Some(crate::daemon::Link::Connected);
        if connected {
            // While a code shows: whether it was used.
            if self.code.borrow().is_some() && ticks.is_multiple_of(2) {
                self.load_status(app);
            } else if ticks.is_multiple_of(10) {
                self.load_clients(app);
            }
        }
        if self.code.borrow().is_some() || expired {
            self.show(app);
        }
    }

    // ---------- Showing ----------

    fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<RemoteArea>();
        area.set_control_others(self.control_others.get());
        area.set_busy(self.busy.get());
        let status = self.status.borrow();
        let enabled = status.as_ref().is_some_and(|s| s.enabled);
        area.set_allow_control(enabled);
        let identity = status.as_ref().and_then(|s| s.identity.clone());
        area.set_identity_name(
            identity
                .as_ref()
                .map(|i| i.name.clone())
                .unwrap_or_default()
                .into(),
        );
        area.set_fingerprint(
            identity
                .as_ref()
                .map(|i| grouped(&i.id))
                .unwrap_or_default()
                .into(),
        );
        let address = status.as_ref().and_then(|s| s.address.clone());
        let shown_address = match (&identity, &address) {
            (Some(identity), Some(address)) => {
                let port = address
                    .rsplit_once(':')
                    .map(|(_, p)| p.to_string())
                    .unwrap_or_default();
                let bound = host_of(address);
                let host = match bound.parse::<IpAddr>() {
                    Ok(ip) if !ip.is_unspecified() => bound.clone(),
                    _ => format!("{}.local", identity.name),
                };
                if port == isocast_protocol::remote::DEFAULT_PORT.to_string() {
                    host
                } else {
                    format!("{host}:{port}")
                }
            }
            _ => String::new(),
        };
        area.set_address(shown_address.into());
        area.set_address_public(
            address
                .as_deref()
                .and_then(|a| host_of(a).parse::<IpAddr>().ok())
                .is_some_and(|ip| !ip.is_unspecified() && !local_ip(ip)),
        );
        area.set_listen_problem(
            status
                .as_ref()
                .and_then(|s| s.error.clone())
                .unwrap_or_default()
                .into(),
        );
        drop(status);
        let code = self.code.borrow();
        match code.as_ref() {
            Some((code, until)) => {
                let (a, b) = code.split_at(code.len().min(3));
                area.set_pair_code(format!("{a} {b}").into());
                area.set_pair_seconds(
                    until.saturating_duration_since(Instant::now()).as_secs() as i32
                );
            }
            None => {
                area.set_pair_code("".into());
                area.set_pair_seconds(0);
            }
        }
        drop(code);
        area.set_pair_problem(self.pair_problem.borrow().as_str().into());

        let me = self.me.borrow().as_ref().map(|(id, _)| id.clone());
        let now = now();
        let rows: Vec<RemotePeerRow> = self
            .clients
            .borrow()
            .iter()
            .map(|peer| {
                let connected = peer.connections.unwrap_or(0) > 0;
                RemotePeerRow {
                    id: peer.id.clone().into(),
                    name: peer.name.clone().into(),
                    detail: if connected {
                        "Connected now".into()
                    } else {
                        match peer.last_seen {
                            Some(at) => format!("Last seen {}", ago(now.saturating_sub(at))).into(),
                            None => "Never connected".into(),
                        }
                    },
                    connected,
                    me: me.as_deref() == Some(peer.id.as_str()),
                }
            })
            .collect();
        let others: Vec<RemotePeerRow> = rows
            .iter()
            .filter(|row| row.connected && !row.me)
            .cloned()
            .collect();
        area.set_presence(match others.len() {
            0 => "".into(),
            n => format!("+{n}").into(),
        });
        area.set_others(model(others));
        area.set_clients(model(rows));

        let roots = self.roots.borrow();
        area.set_folders_custom(roots.as_ref().is_some_and(|r| r.custom));
        area.set_folders(model(roots.iter().flat_map(|r| r.roots.iter()).map(
            |root| {
                let why = match root.why.as_str() {
                    "chosen" => "Chosen",
                    "default" => "Default",
                    "recordings" => "Recordings",
                    "fonts" => "Fonts (read only)",
                    other => other,
                };
                RemoteFolderRow {
                    path: crate::platform::abbreviate_home(&root.path.to_string_lossy()).into(),
                    why: why.into(),
                    removable: matches!(root.why.as_str(), "chosen" | "default"),
                }
            },
        )));
        drop(roots);

        area.set_paired(model(self.paired.borrow().iter().map(|peer| {
            RemotePeerRow {
                id: peer.id.clone().into(),
                name: peer.name.clone().into(),
                detail: peer
                    .address
                    .as_deref()
                    .unwrap_or("")
                    .trim_start_matches("tls://")
                    .into(),
                connected: false,
                me: false,
            }
        })));
    }

    // ---------- Settings → Remote ----------

    fn set_control_others(&self, app: &Rc<App>, on: bool) {
        self.control_others.set(on);
        if !app.mode.offscreen {
            save_control_others(on);
        }
        if on {
            self.load_paired();
            self.start_discovery();
        } else {
            *self.watch.borrow_mut() = None;
            self.found.borrow_mut().clear();
        }
        self.show(app);
    }

    fn set_allow_control(&self, app: &Rc<App>, on: bool) {
        if self.remote_window.get() || self.busy.get() {
            return;
        }
        self.busy.set(true);
        self.show(app);
        let method = if on {
            Method::RemoteEnable(None)
        } else {
            Method::RemoteDisable
        };
        app.call_as::<RemoteStatus>(method, move |app, result| {
            let remote = &app.remote;
            remote.busy.set(false);
            match result {
                Ok(status) => {
                    if !status.enabled {
                        *remote.code.borrow_mut() = None;
                        remote.clients.borrow_mut().clear();
                    }
                    *remote.status.borrow_mut() = Some(status);
                    if on {
                        remote.load_clients(app);
                        remote.load_roots(app);
                    }
                }
                Err(error) => app.failed(
                    if on {
                        "Couldn’t let other computers in"
                    } else {
                        "Couldn’t turn remote control off"
                    },
                    &error,
                ),
            }
            remote.show(app);
        });
    }

    fn pair_begin(&self, app: &Rc<App>) {
        self.pair_problem.borrow_mut().clear();
        app.call_as::<RemotePairWindow>(Method::RemotePairBegin, |app, result| {
            let remote = &app.remote;
            match result {
                Ok(window) => {
                    let until =
                        Instant::now() + Duration::from_secs(window.expires_in_seconds.into());
                    *remote.code.borrow_mut() = Some((window.code.clone(), until));
                }
                Err(error) => *remote.pair_problem.borrow_mut() = error.message,
            }
            remote.show(app);
        });
    }

    fn revoke(&self, app: &Rc<App>, id: &str) {
        let name = self
            .clients
            .borrow()
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        app.call_as::<Vec<RemotePeer>>(
            Method::RemoteRevoke(RemoteRevokeParams {
                client_id: id.into(),
            }),
            move |app, result| match result {
                Ok(clients) => {
                    *app.remote.clients.borrow_mut() = clients;
                    app.toast(
                        ToastKind::Success,
                        &format!("Revoked {name}"),
                        "It was cut off and has to pair again to control this engine.",
                    );
                    app.remote.load_clients(app);
                    app.remote.show(app);
                }
                Err(error) => app.failed("Couldn’t revoke it", &error),
            },
        );
    }

    fn forget(&self, app: &Rc<App>, id: &str) {
        match isocast_client::remote::forget(id) {
            Ok(()) => self.load_paired(),
            Err(error) => {
                app.toast(ToastKind::Error, "Couldn’t forget it", format!("{error:#}"));
            }
        }
        self.show(app);
    }

    /// The chosen folders now (the defaults until some were chosen).
    fn chosen_folders(&self) -> Vec<String> {
        self.roots
            .borrow()
            .iter()
            .flat_map(|r| r.roots.iter())
            .filter(|root| matches!(root.why.as_str(), "chosen" | "default"))
            .map(|root| root.path.to_string_lossy().into_owned())
            .collect()
    }

    fn add_folder(&self, app: &Rc<App>) {
        let add = |app: &Rc<App>, chosen: Option<Vec<String>>| {
            if let Some(folder) = chosen.and_then(|c| c.into_iter().next()) {
                let mut folders = app.remote.chosen_folders();
                if !folders.contains(&folder) {
                    folders.push(folder);
                }
                app.remote.set_folders(app, Some(folders));
            }
        };
        if app.mode.offscreen {
            return;
        }
        let options = Choose {
            folders: true,
            prompt: "Add",
            ..Choose::default()
        };
        choose_here(app, options, move |app, chosen| match chosen {
            Some(chosen) => add(app, Some(chosen)),
            // No open panel here (Windows): this engine's own disk, from
            // the home folder.
            None => {
                let home = isocast_protocol::api::home_folder()
                    .to_string_lossy()
                    .into_owned();
                let mut roots = vec![(home.clone(), "Home".to_string())];
                if cfg!(windows) {
                    roots.push((r"C:\".into(), "C:".into()));
                }
                app.remote.open_browser(
                    app,
                    "Add a Folder",
                    "Add",
                    roots,
                    &home,
                    true,
                    Vec::new(),
                    Box::new(add),
                );
            }
        });
    }

    fn remove_folder(&self, app: &Rc<App>, shown: &str) {
        let folders: Vec<String> = self
            .chosen_folders()
            .into_iter()
            .filter(|f| crate::platform::abbreviate_home(f) != shown && f != shown)
            .collect();
        self.set_folders(app, Some(folders));
    }

    fn set_folders(&self, app: &Rc<App>, folders: Option<Vec<String>>) {
        app.call_as::<FolderRoots>(
            Method::FolderRootsSet(FolderRootsSetParams { folders }),
            |app, result| match result {
                Ok(roots) => {
                    *app.remote.roots.borrow_mut() = Some(roots);
                    app.remote.show(app);
                }
                Err(error) => app.failed("Couldn’t change the folders", &error),
            },
        );
    }

    // ---------- The engine picker ----------

    fn picker_menu(&self, app: &Rc<App>) -> MenuBuilder {
        let (machine, _) = this_machine();
        let current = seam::window_engine();
        let mut menu = MenuBuilder::new();
        if self.control_others.get() || self.remote_window.get() {
            menu = menu
                .heading("Engines")
                .item("engine:local", machine)
                .checked(current.is_local());
            for peer in self.paired.borrow().iter() {
                let checked = matches!(&current, Engine::Remote { target } if *target == peer.id || *target == peer.name);
                menu = menu
                    .item(&format!("engine:open:{}", peer.id), &peer.name)
                    .checked(checked);
            }
            let paired_ids: Vec<String> =
                self.paired.borrow().iter().map(|p| p.id.clone()).collect();
            let found: Vec<_> = self
                .found
                .borrow()
                .values()
                .filter(|f| !paired_ids.contains(&f.txt.id))
                .filter(|f| {
                    // Not this machine's own engine.
                    self.status
                        .borrow()
                        .as_ref()
                        .and_then(|s| s.identity.as_ref())
                        .is_none_or(|i| i.id != f.txt.id)
                })
                .cloned()
                .collect();
            if !found.is_empty() {
                menu = menu.separator().heading("On This Network");
                for f in found {
                    let name = if f.txt.name.is_empty() {
                        &f.name
                    } else {
                        &f.txt.name
                    };
                    menu = menu
                        .item(&format!("engine:pair:{}", f.name), &format!("{name}…"))
                        .hint("Pair");
                }
            }
            if self.control_others.get() {
                menu = menu.separator().item("engine:add", "Add Engine…");
            }
        }
        let others = self.others_names(app);
        if !others.is_empty() {
            menu = menu.separator().heading("Also Connected");
            for (name, detail) in others {
                menu = menu
                    .item("engine:other", &name)
                    .hint(&detail)
                    .disabled(true);
            }
        }
        if !self.remote_window.get() {
            menu = menu.separator().item("engine:settings", "Remote Settings…");
        }
        menu
    }

    fn others_names(&self, _app: &Rc<App>) -> Vec<(String, String)> {
        let me = self.me.borrow().as_ref().map(|(id, _)| id.clone());
        self.clients
            .borrow()
            .iter()
            .filter(|c| c.connections.unwrap_or(0) > 0 && me.as_deref() != Some(c.id.as_str()))
            .map(|c| (c.name.clone(), "Connected now".to_string()))
            .collect()
    }

    // ---------- Pairing with an engine ----------

    fn open_pairing(&self, app: &Rc<App>, found: Option<isocast_discovery::Found>) {
        let mut pairing = Pairing {
            step: "address".into(),
            ..Pairing::default()
        };
        if let Some(found) = found {
            pairing.step = "code".into();
            pairing.name = if found.txt.name.is_empty() {
                found.name.clone()
            } else {
                found.txt.name.clone()
            };
            // Its host name (the OS resolves it to every address it has,
            // and it survives DHCP), else the first address it gave.
            let host = found.host.trim_end_matches('.');
            pairing.address = if host.is_empty() {
                found
                    .addrs
                    .first()
                    .map(|a| a.to_string())
                    .unwrap_or_default()
            } else {
                format!("{host}:{}", found.port)
            };
        }
        *self.pairing.borrow_mut() = pairing;
        if let Some(window) = app.window() {
            let area = window.global::<RemoteArea>();
            area.set_pairing_typed("".into());
            area.set_pairing_confirmed(false);
        }
        self.show_pairing(app);
    }

    fn show_pairing(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<RemoteArea>();
        let p = self.pairing.borrow();
        area.set_pairing_step(p.step.as_str().into());
        area.set_pairing_address(p.address.as_str().into());
        area.set_pairing_name(p.name.as_str().into());
        area.set_pairing_public(p.public);
        area.set_pairing_busy(p.busy);
        area.set_pairing_error(p.error.as_str().into());
        area.set_pairing_shown(true);
    }

    fn pairing_next(&self, app: &Rc<App>, address: &str) {
        let address = address.trim().to_string();
        if address.is_empty() {
            return;
        }
        let confirmed = app
            .window()
            .is_some_and(|w| w.global::<RemoteArea>().get_pairing_confirmed());
        {
            let mut p = self.pairing.borrow_mut();
            p.address = address.clone();
            p.error.clear();
            p.busy = true;
        }
        self.show_pairing(app);
        let probe = address.clone();
        background(
            move || address_is_local(&probe),
            move |app, local| {
                let remote = &app.remote;
                {
                    let mut p = remote.pairing.borrow_mut();
                    p.busy = false;
                    if !local && !confirmed {
                        p.public = true;
                    } else {
                        p.step = "code".into();
                        p.name = host_of(&address).trim_end_matches(".local").to_string();
                    }
                }
                remote.show_pairing(app);
            },
        );
    }

    fn pairing_code(&self, app: &Rc<App>, code: &str) {
        let code: String = code.chars().filter(char::is_ascii_digit).collect();
        let (address, name) = {
            let mut p = self.pairing.borrow_mut();
            p.busy = true;
            p.error.clear();
            (p.address.clone(), p.name.clone())
        };
        self.show_pairing(app);
        background(
            move || {
                isocast_client::remote::pair(&address, &code, None).map_err(|e| format!("{e:#}"))
            },
            move |app, result| {
                let remote = &app.remote;
                {
                    let mut p = remote.pairing.borrow_mut();
                    p.busy = false;
                    match result {
                        Ok(peer) => {
                            p.step = "done".into();
                            p.name = peer.name.clone();
                            p.paired = Some(peer.id);
                        }
                        Err(message) => {
                            p.error = pair_error(&message, &name);
                            // A code allows one try: the next is typed anew.
                            if let Some(window) = app.window() {
                                window.global::<RemoteArea>().set_pairing_typed("".into());
                            }
                        }
                    }
                }
                remote.load_paired();
                remote.show(app);
                remote.show_pairing(app);
            },
        );
    }

    fn pairing_finish(&self, app: &Rc<App>) {
        let paired = self.pairing.borrow_mut().paired.take();
        *self.pairing.borrow_mut() = Pairing::default();
        if let Some(window) = app.window() {
            window.global::<RemoteArea>().set_pairing_shown(false);
        }
        if let Some(id) = paired {
            self.open_window(app, Engine::Remote { target: id });
        }
    }

    // ---------- The engine file browser ----------

    #[expect(
        clippy::too_many_arguments,
        reason = "one call site per kind of picker"
    )]
    fn open_browser(
        &self,
        app: &Rc<App>,
        title: &str,
        prompt: &str,
        roots: Vec<(String, String)>,
        start: &str,
        folders: bool,
        extensions: Vec<String>,
        done: Chosen,
    ) {
        let note = if folders {
            "Folders".to_string()
        } else if extensions.is_empty() {
            "All files".to_string()
        } else {
            extensions
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        *self.browser.borrow_mut() = Some(Browser {
            desktop_fallback: false,
            roots,
            path: String::new(),
            parent: None,
            entries: Vec::new(),
            selected: String::new(),
            folders,
            show_only: false,
            extensions,
            note,
            busy: true,
            done: Some(done),
            home: match self.remote_window.get() {
                true => String::new(),
                false => isocast_protocol::api::home_folder()
                    .to_string_lossy()
                    .into_owned(),
            },
        });
        if let Some(window) = app.window() {
            let area = window.global::<RemoteArea>();
            area.set_browser_title(title.into());
            area.set_browser_prompt(prompt.into());
        }
        self.show_browser(app);
        if !start.is_empty() {
            self.list(app, start, None);
        }
    }

    /// List `path` in the browser, selecting `select` once listed.
    fn list(&self, app: &Rc<App>, path: &str, select: Option<String>) {
        let (extensions, folders) = match self.browser.borrow_mut().as_mut() {
            Some(b) => {
                b.busy = true;
                (b.extensions.clone(), b.folders)
            }
            None => return,
        };
        self.show_browser(app);
        let params = FilesListParams {
            path: PathBuf::from(path),
            extensions: (!extensions.is_empty()).then_some(extensions),
            folders_only: folders,
        };
        let show = move |app: &Rc<App>, result: Result<FilesList, crate::daemon::CallError>| {
            let remote = &app.remote;
            if let Some(b) = remote.browser.borrow_mut().as_mut() {
                b.busy = false;
                match result {
                    Ok(list) => {
                        b.path = list.path.to_string_lossy().into_owned();
                        b.parent = list.parent.map(|p| p.to_string_lossy().into_owned());
                        b.selected = select.unwrap_or_default();
                        b.entries = list
                            .entries
                            .iter()
                            .map(|e| RemoteFileRow {
                                name: e.name.clone().into(),
                                path: e.path.to_string_lossy().into_owned().into(),
                                folder: e.folder,
                                detail: e.size.map(size_text).unwrap_or_default().into(),
                            })
                            .collect();
                        if list.truncated {
                            b.note = format!("Only the first {} shown", b.entries.len());
                        }
                    }
                    Err(error) => b.note = error.message,
                }
            }
            remote.show_browser(app);
        };
        #[cfg(target_os = "linux")]
        if self.remote_window.get()
            && self
                .browser
                .borrow()
                .as_ref()
                .is_some_and(|b| b.desktop_fallback)
        {
            // A client-side import in a remote window must never read a path
            // supplied by the remote engine. List with this computer's engine.
            background(
                move || {
                    isocast_client::Client::connect_local_or_launch()?
                        .call(Method::FilesList(params))
                },
                move |app, result| {
                    show(
                        app,
                        result.map_err(|err| crate::daemon::CallError {
                            message: err.to_string(),
                            code: None,
                        }),
                    )
                },
            );
            return;
        }
        app.call_as::<FilesList>(Method::FilesList(params), show);
    }

    fn open_entry(&self, app: &Rc<App>, path: &str) {
        let folder = self
            .browser
            .borrow()
            .as_ref()
            .and_then(|b| b.entries.iter().find(|e| e.path == path).map(|e| e.folder));
        match folder {
            Some(true) => self.list(app, path, None),
            Some(false) => {
                if let Some(b) = self.browser.borrow_mut().as_mut() {
                    b.selected = path.into();
                }
                self.browser_choose(app);
            }
            None => {}
        }
    }

    fn browser_choose(&self, app: &Rc<App>) {
        let chosen = self.browser.borrow().as_ref().map(|b| {
            if b.show_only {
                Vec::new()
            } else if b.folders {
                // The folder selected in the list, else the one shown.
                let selected_folder = b
                    .entries
                    .iter()
                    .find(|e| e.folder && e.path == b.selected.as_str())
                    .map(|e| e.path.to_string());
                vec![selected_folder.unwrap_or_else(|| b.path.clone())]
            } else {
                vec![b.selected.clone()]
            }
        });
        let chosen = chosen.filter(|c| c.iter().all(|p| !p.is_empty()));
        self.browser_close(app, chosen);
    }

    fn browser_close(&self, app: &Rc<App>, chosen: Option<Vec<String>>) {
        let done = self
            .browser
            .borrow_mut()
            .take()
            .and_then(|mut b| b.done.take());
        if let Some(window) = app.window() {
            window.global::<RemoteArea>().set_browser_open(false);
        }
        if let Some(done) = done {
            done(app, Some(chosen.unwrap_or_default()));
        }
    }

    fn show_browser(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<RemoteArea>();
        let browser = self.browser.borrow();
        let Some(b) = browser.as_ref() else {
            area.set_browser_open(false);
            return;
        };
        let place = if self.remote_window.get() && !b.desktop_fallback {
            area.get_engine_name().to_string()
        } else {
            this_machine().0.to_string()
        };
        area.set_browser_where(place.as_str().into());
        let group = format!("On {place}");
        area.set_browser_roots(model(b.roots.iter().enumerate().map(
            |(i, (path, name))| NavTab {
                id: path.as_str().into(),
                label: name.as_str().into(),
                icon: "folder".into(),
                group: if i == 0 {
                    group.as_str().into()
                } else {
                    "".into()
                },
            },
        )));
        let root = b
            .roots
            .iter()
            .filter(|(path, _)| engine_path::within(&b.path, path))
            .max_by_key(|(path, _)| path.len())
            .map(|(path, _)| path.clone())
            .unwrap_or_default();
        area.set_browser_root(root.into());
        area.set_browser_path(engine_path::abbreviate(&b.path, &b.home).into());
        area.set_browser_has_parent(b.parent.is_some());
        area.set_browser_entries(model(b.entries.iter().cloned()));
        area.set_browser_selected(b.selected.as_str().into());
        area.set_browser_folders(b.folders);
        area.set_browser_show_only(b.show_only);
        area.set_browser_note(if b.desktop_fallback {
            format!(
                "A desktop portal provides the native file dialog.  {}",
                b.note
            )
            .into()
        } else {
            b.note.as_str().into()
        });
        area.set_browser_busy(b.busy);
        area.set_browser_open(true);
    }

    // ---------- Snapshots ----------

    fn sample(&self, app: &Rc<App>) {
        self.sampled.set(true);
        self.control_others.set(true);
        let now = now();
        *self.status.borrow_mut() = Some(RemoteStatus {
            enabled: true,
            running: true,
            identity: Some(isocast_protocol::remote::RemoteIdentity {
                id: "6f1c0a93d2e84b7f5a0c9e13b4d27f86a1e09c5d3b7f2e48c6a1d90b3e5f7a24".into(),
                name: "basebook".into(),
                spki: String::new(),
            }),
            address: Some("0.0.0.0:47470".into()),
            pairing_open: true,
            error: None,
        });
        *self.code.borrow_mut() =
            Some(("482913".into(), Instant::now() + Duration::from_secs(102)));
        let peer = |id: &str, name: &str, connections: u32, last_seen: Option<u64>| RemotePeer {
            id: id.into(),
            name: name.into(),
            spki: String::new(),
            paired_at: now - 86400,
            last_seen,
            address: None,
            connections: Some(connections),
        };
        *self.clients.borrow_mut() = vec![
            peer("a1", "exon-desk", 3, Some(now)),
            peer("b2", "exonbook", 0, Some(now - 1800)),
        ];
        let home = isocast_protocol::api::home_folder();
        let root = |path: &str, why: &str, read_only: bool| isocast_protocol::roots::FolderRoot {
            path: home.join(path),
            read_only,
            why: why.into(),
        };
        *self.roots.borrow_mut() = Some(FolderRoots {
            roots: vec![
                root("Movies", "default", false),
                root("Pictures", "default", false),
                root("Music", "default", false),
                root("Desktop", "default", false),
                root("Downloads", "default", false),
                root("Movies/Isocast", "recordings", false),
                isocast_protocol::roots::FolderRoot {
                    path: "/Library/Fonts".into(),
                    read_only: true,
                    why: "fonts".into(),
                },
            ],
            custom: false,
        });
        *self.paired.borrow_mut() = vec![RemotePeer {
            address: Some("tls://10.0.10.149:47470".into()),
            ..peer("c3", "exon-desk", 0, None)
        }];
        self.show(app);
    }
}

/// "84.2 MB", "212 KB".
fn size_text(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

// ---------- For the other areas ----------

/// Whether `app`'s window controls another computer's engine.
pub fn is_remote(app: &App) -> bool {
    app.remote.remote_window.get()
}

/// Ask for files (or a folder) to use on the engine: the system's open
/// panel for this machine's engine (on Linux the desktop portal's, else
/// the engine file browser); the engine file browser for another
/// computer's (its disk, inside the folders other computers can use).
/// `done` gets the paths (empty if cancelled), or none where there is no
/// panel, as [`crate::platform::choose`] answers.
pub fn choose(
    app: &Rc<App>,
    options: Choose,
    done: impl FnOnce(&Rc<App>, Option<Vec<String>>) + 'static,
) {
    if !is_remote(app) {
        return choose_here(app, options, done);
    }
    let title = if options.folders {
        "Choose a Folder".to_string()
    } else {
        format!("Choose on {}", app.remote.engine_name(app))
    };
    let prompt = if options.prompt.is_empty() {
        "Choose".to_string()
    } else {
        options.prompt.to_string()
    };
    let extensions: Vec<String> = options.types.iter().map(|t| t.to_string()).collect();
    let folders = options.folders;
    browse_engine(app, &title, &prompt, folders, extensions, Box::new(done));
}

/// Ask for files on this computer (to read here, not on the engine): the
/// system's open panel, the desktop portal's on Linux; `done` gets none
/// where there is no panel, as [`crate::platform::choose`] answers.
pub fn choose_here(
    app: &Rc<App>,
    options: Choose,
    done: impl FnOnce(&Rc<App>, Option<Vec<String>>) + 'static,
) {
    #[cfg(target_os = "linux")]
    {
        let folders = options.folders;
        let dialog = crate::platform::PortalDialog::Open {
            folders,
            multiple: options.multiple,
        };
        let extensions = options.types.iter().map(|s| s.to_string()).collect();
        let title = if folders {
            "Choose a Folder"
        } else {
            "Choose a File"
        };
        let prompt = if options.prompt.is_empty() {
            "Choose"
        } else {
            options.prompt
        }
        .to_string();
        let app = Rc::downgrade(app);
        crate::platform::ask_portal(dialog, options.prompt, options.types, move |paths| {
            let Some(app) = app.upgrade() else { return };
            if paths.is_some() {
                return done(&app, paths);
            }
            if !is_remote(&app) && !folders {
                browse_engine(&app, title, &prompt, folders, extensions, Box::new(done));
            } else {
                let home = isocast_protocol::api::home_folder()
                    .to_string_lossy()
                    .into_owned();
                app.remote.open_browser(
                    &app,
                    title,
                    &prompt,
                    vec![(home.clone(), "Home".into())],
                    "",
                    folders,
                    extensions,
                    Box::new(done),
                );
                if let Some(b) = app.remote.browser.borrow_mut().as_mut() {
                    b.desktop_fallback = true;
                    b.home = home.clone();
                }
                app.remote.list(&app, &home, None);
                return;
            }
            if let Some(b) = app.remote.browser.borrow_mut().as_mut() {
                b.desktop_fallback = true;
            }
            app.remote.show_browser(&app);
        });
    }
    #[cfg(not(target_os = "linux"))]
    done(app, crate::platform::choose(options));
}

/// The engine file browser (its disk, inside the folders other computers
/// can use), for `choose`.
fn browse_engine(
    app: &Rc<App>,
    title: &str,
    prompt: &str,
    folders: bool,
    extensions: Vec<String>,
    done: Chosen,
) {
    // Its roots first, then the first one listed.
    app.remote.open_browser(
        app,
        title,
        prompt,
        Vec::new(),
        "",
        folders,
        extensions,
        done,
    );
    app.call_as::<FilesRoots>(Method::FilesRoots, |app, result| {
        let remote = &app.remote;
        match result {
            Ok(roots) => {
                let home = roots.home.to_string_lossy().into_owned();
                let list: Vec<(String, String)> = roots
                    .roots
                    .iter()
                    .filter(|r| !r.read_only || r.why == "fonts")
                    .map(|r| {
                        let path = r.path.to_string_lossy().into_owned();
                        let name = engine_path::name(&path).to_string();
                        let name = if r.why == "recordings" && !engine_path::within(&path, &home) {
                            path.clone()
                        } else {
                            name
                        };
                        (path, name)
                    })
                    .collect();
                let first = list.first().map(|(p, _)| p.clone());
                if let Some(b) = remote.browser.borrow_mut().as_mut() {
                    b.roots = list;
                    b.home = home.clone();
                    b.busy = false;
                    if first.is_none() {
                        b.note =
                            "No folders are shared with other computers on this engine.".into();
                    }
                }
                remote.show_browser(app);
                if let Some(first) = first {
                    remote.list(app, &first, None);
                }
            }
            Err(error) => {
                if let Some(b) = remote.browser.borrow_mut().as_mut() {
                    b.busy = false;
                    b.note = error.message;
                }
                remote.show_browser(app);
            }
        }
    });
}

/// Paths on the engine, which may be another OS's: split on both `\` and
/// `/`, and "~" is the engine's home, not this machine's.
mod engine_path {
    fn separator(c: char) -> bool {
        c == '/' || c == '\\'
    }

    /// The last part ("Videos" of `C:\Users\Ryan\Videos`), or the whole
    /// path for a root (`C:\`, `/`).
    pub fn name(path: &str) -> &str {
        let trimmed = path.trim_end_matches(separator);
        match trimmed.rsplit(separator).next() {
            Some(last) if !last.is_empty() && last.len() < trimmed.len() => last,
            _ => path,
        }
    }

    /// The folder holding `path`, if it has one (`C:\` for `C:\Videos`).
    pub fn parent(path: &str) -> Option<&str> {
        let trimmed = path.trim_end_matches(separator);
        let at = trimmed.rfind(separator)?;
        // Keep the root's separator (`C:\`, `/`).
        let keep = match trimmed[..at].is_empty() || trimmed[..at].ends_with(':') {
            true => at + 1,
            false => at,
        };
        Some(&path[..keep])
    }

    /// Whether `path` is `root` or inside it.
    pub fn within(path: &str, root: &str) -> bool {
        let root = root.trim_end_matches(separator);
        if root.is_empty() {
            return path.starts_with(separator);
        }
        // Windows paths ignore case.
        let windows = root.contains('\\') || root.as_bytes().get(1) == Some(&b':');
        let same = |a: &str, b: &str| match windows {
            true => a.eq_ignore_ascii_case(b),
            false => a == b,
        };
        match path.get(..root.len()) {
            Some(start) if same(start, root) => {
                path.len() == root.len() || path[root.len()..].starts_with(separator)
            }
            _ => false,
        }
    }

    /// `path` with the engine's `home` as "~".
    pub fn abbreviate(path: &str, home: &str) -> String {
        let home = home.trim_end_matches(separator);
        if home.is_empty() || !within(path, home) {
            return path.to_string();
        }
        format!("~{}", &path[home.len()..])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn windows_paths_from_a_mac_and_back() {
            assert_eq!(name(r"C:\Users\Ryan\Videos"), "Videos");
            assert_eq!(name(r"C:\Users\Ryan\Videos\"), "Videos");
            assert_eq!(name(r"C:\"), r"C:\");
            assert_eq!(name("/Users/ryan/Movies"), "Movies");
            assert_eq!(name("/"), "/");
            assert_eq!(parent(r"C:\Users\Ryan\a.mov"), Some(r"C:\Users\Ryan"));
            assert_eq!(parent(r"C:\Videos"), Some(r"C:\"));
            assert_eq!(parent("/Users"), Some("/"));
            assert_eq!(parent("a.mov"), None);
            assert!(within(r"C:\Users\Ryan\Videos\a.mov", r"c:\users\ryan\"));
            assert!(!within(r"C:\Users\Ryanne", r"C:\Users\Ryan"));
            assert!(within("/Users/ryan", "/Users/ryan"));
            assert!(!within("/Users/Ryan/x", "/Users/ryan"));
            assert_eq!(
                abbreviate(r"C:\Users\Ryan\Videos", r"C:\Users\Ryan"),
                r"~\Videos"
            );
            assert_eq!(abbreviate("/tmp/x", "/Users/ryan"), "/tmp/x");
            assert_eq!(abbreviate("/tmp/x", ""), "/tmp/x");
        }
    }
}

/// "Show in Finder" for a path on the engine: the system's file manager
/// for this machine's engine; for another computer's, the engine file
/// browser showing it ("Show on <engine>").
pub fn reveal(app: &Rc<App>, path: &str) {
    if !is_remote(app) {
        crate::platform::reveal(path);
        return;
    }
    let name = app.remote.engine_name(app);
    // The engine's path, which may be another OS's.
    let folder = engine_path::parent(path).unwrap_or(path).to_string();
    app.remote.open_browser(
        app,
        &format!("Show on {name}"),
        "Done",
        Vec::new(),
        "",
        false,
        Vec::new(),
        Box::new(|_, _| {}),
    );
    if let Some(b) = app.remote.browser.borrow_mut().as_mut() {
        b.show_only = true;
        b.note = engine_path::abbreviate(path, &b.home);
    }
    app.remote.list(app, &folder, Some(path.to_string()));
}

/// The words for "Show in Finder" on `app`'s window: "Show on exon-desk"
/// for another computer's engine.
pub fn reveal_label(app: &App) -> String {
    if is_remote(app) {
        let name = app
            .window()
            .map(|w| w.global::<RemoteArea>().get_engine_name().to_string())
            .unwrap_or_default();
        format!("Show on {name}")
    } else {
        crate::engine::words().show_in_files.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_are_private_link_local_loopback_and_tailscale() {
        for ip in [
            "10.0.10.26",
            "192.168.1.4",
            "172.16.0.1",
            "169.254.3.3",
            "127.0.0.1",
            "100.78.27.26",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(local_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.128.0.1", "2606:4700::1", "::ffff:1.1.1.1"] {
            assert!(!local_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn hosts_without_scheme_or_port() {
        assert_eq!(host_of("tls://10.0.10.149:47470"), "10.0.10.149");
        assert_eq!(host_of(" exon-desk.local "), "exon-desk.local");
        assert_eq!(host_of("exon-desk.local:47470"), "exon-desk.local");
        assert_eq!(host_of("[fe80::1]:47470"), "fe80::1");
        assert_eq!(host_of("fe80::1"), "fe80::1");
        assert_eq!(host_of("[fd00::2]"), "fd00::2");
    }

    #[test]
    fn literal_and_dot_local_addresses_need_no_lookup() {
        assert!(address_is_local("exon-desk.local"));
        assert!(address_is_local("tls://10.0.10.149:47470"));
        assert!(!address_is_local("8.8.8.8:47470"));
    }

    #[test]
    fn pairing_errors_say_what_to_do() {
        let wrong = pair_error("pairing confirmation failed", "exon-desk");
        assert!(wrong.contains("didn't match") && wrong.contains("exon-desk"));
        assert!(pair_error("the code expired", "x").contains("expired"));
        assert!(pair_error("timed out waiting", "x").contains("expired"));
        assert!(pair_error("expected six digits", "x").contains("6 digits"));
        assert!(pair_error("identity changed", "x").contains("Forget it"));
        let unreachable = pair_error("could not connect to engine: refused", "x");
        assert!(unreachable.contains("Couldn't reach x") && unreachable.contains("refused"));
        assert_eq!(pair_error("something else", "x"), "something else");
    }

    #[test]
    fn only_a_refused_key_reads_as_revoked() {
        assert!(trust_refused("received fatal alert: BadCertificate"));
        assert!(trust_refused("received fatal alert: AccessDenied"));
        assert!(!trust_refused("TLS handshake deadline"));
        assert!(!trust_refused("TLS handshake closed"));
        assert!(!trust_refused("Connection refused (os error 61)"));
    }
}
