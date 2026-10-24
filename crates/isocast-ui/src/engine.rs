//! Which engine this window controls (docs/remote-engines.md, decisions 5,
//! 21 and 22): this machine's own (`Engine::Local`) or another computer's
//! (`Engine::Remote`), chosen once when the app starts and never changed.
//! One window is one engine; another engine opens another window
//! ([`open_window`]), which runs as a process of its own.
//!
//! Every connection the app makes goes through [`isocast_client::Engine`]
//! (selected here, so `Client::connect*` reach this window's engine), and
//! what may only touch this machine's engine checks [`is_local`]: the
//! stale-engine restart and both updaters (src/update*.rs), Restart Engine,
//! `shutdown_if_idle`, and launching an engine at all (src/daemon.rs). A
//! remote window never starts, stops or restarts the engine on this machine.
//!
//! The engine's words (decision 22) follow its operating system: this
//! machine's for a local window, `hello.os` for a remote one once it has
//! answered ([`set_hello`]); until then the app's own.

use std::sync::{OnceLock, RwLock};

use anyhow::{Context, Result};
use isocast_client::Engine;
use isocast_protocol::hello::{EngineOs, Hello};
use isocast_protocol::platform_text::{self, Words};

static ENGINE: OnceLock<Engine> = OnceLock::new();
/// What a remote engine said about itself (`hello`), once it has.
static HELLO: RwLock<Option<Hello>> = RwLock::new(None);

/// Choose the window's engine (`--engine`, else `$ISOCAST_ENGINE`, else
/// this machine's), once, before any thread connects.
pub fn select(target: Option<&str>) -> Result<()> {
    let engine = match target {
        Some(target) => Engine::parse(target)?,
        None => Engine::selected()?,
    };
    isocast_client::Engine::select(engine.clone())?;
    ENGINE
        .set(engine)
        .map_err(|_| anyhow::anyhow!("the window's engine was already chosen"))
}

/// The window's engine.
pub fn current() -> Engine {
    ENGINE
        .get()
        .cloned()
        .unwrap_or_else(|| Engine::selected().unwrap_or_default())
}

/// This machine's own engine: the only one the app may launch, stop,
/// restart or update.
pub fn is_local() -> bool {
    current().is_local()
}

/// Remember what a remote engine said about itself.
pub fn set_hello(hello: Hello) {
    *HELLO.write().unwrap() = Some(hello);
}

/// The remote engine's `hello`, once it has answered.
pub fn hello() -> Option<Hello> {
    HELLO.read().unwrap().clone()
}

/// The engine machine's operating system. A local window's is this
/// machine's; a remote one's is what `hello` said, else the app's until
/// then.
pub fn os() -> EngineOs {
    let own = EngineOs::current();
    if is_local() {
        return own;
    }
    hello().map_or(own, |hello| hello.os)
}

/// Whether the engine's conventions are Windows' (the Slint side's
/// `PlatformText.engine-windows`): off macOS, Linux's too, as
/// `platform_text::pick` has them.
pub fn windows() -> bool {
    !matches!(os(), EngineOs::Macos)
}

/// The engine machine's nouns: Keychain or Credential Manager, Finder or
/// Explorer, "Mac" or "PC".
pub fn words() -> Words {
    Words::for_os(os())
}

/// `mac` or `windows`, by the engine machine's operating system.
pub fn pick(mac: &'static str, windows: &'static str) -> &'static str {
    platform_text::pick(os(), mac, windows)
}

/// The engine's name as people know it ("exon-desk"), for a remote window:
/// its own (from `hello`), else the name it was paired under, else the
/// target given. `None` for this machine's engine.
pub fn name() -> Option<String> {
    let Engine::Remote { target } = current() else {
        return None;
    };
    if let Some(hello) = hello() {
        return Some(hello.name);
    }
    // The paired engines' file, read once: the window's engine doesn't
    // change, and `hello` names it from then on.
    static PAIRED: OnceLock<Option<String>> = OnceLock::new();
    let paired = PAIRED.get_or_init(|| {
        isocast_client::remote::engines().ok().and_then(|peers| {
            peers
                .into_iter()
                .find(|peer| peer.id == target || peer.name == target)
                .map(|peer| peer.name)
        })
    });
    Some(
        paired
            .clone()
            .unwrap_or_else(|| target.trim_start_matches("tls://").to_string()),
    )
}

/// The `owner` this app's projectors carry (decision 24): none for this
/// machine's engine, this client's identity for another computer's.
pub fn own_projector_owner() -> Option<String> {
    static OWN: OnceLock<Option<String>> = OnceLock::new();
    if is_local() {
        return None;
    }
    OWN.get_or_init(|| {
        isocast_client::remote::identity()
            .ok()
            .map(|identity| identity.id)
    })
    .clone()
}

/// Open a window on `engine`, beside this one: a new process of this app
/// with `--engine` (none for this machine's engine). The new window
/// connects on its own; this one is untouched.
pub fn open_window(engine: &Engine) -> Result<()> {
    let exe = std::env::current_exe().context("could not find the app to open another window")?;
    let mut command = std::process::Command::new(exe);
    // A remote window's process may have been given the engine this way;
    // the new one is told explicitly instead.
    command.env_remove("ISOCAST_ENGINE");
    if let Engine::Remote { target } = engine {
        command.arg("--engine").arg(target);
    } else {
        command.arg("--engine").arg("local");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command
        .spawn()
        .context("could not open another Isocast window")?;
    // Reap it when it closes (the window runs on its own).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
