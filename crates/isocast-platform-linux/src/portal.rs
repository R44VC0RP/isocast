//! A single user-selected portal stream per display or window source. The
//! portal owns consent and cursor compositing; discovery never opens a
//! share dialog.
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use ashpd::enumflags2::BitFlags;
use futures_util::StreamExt;
use isocast_engine::{CaptureFormat, PickedScreen, ScreenTarget, VideoSink};
use isocast_protocol::devices::Display;
use serde::{Deserialize, Serialize};

use crate::{frames::Frame, pipewire_video};

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
static DECLINED: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());
const MAX_SETTINGS: u64 = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    token: Option<String>,
    width: u32,
    height: u32,
}

fn settings() -> std::path::PathBuf {
    isocast_protocol::api::setup_path().with_extension("linux-capture.json")
}

fn load() -> Result<BTreeMap<String, Saved>> {
    let file = match std::fs::File::open(settings()) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        file.metadata()?.len() <= MAX_SETTINGS,
        "Linux capture settings exceed 64 KiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_SETTINGS as usize,
        "Linux capture settings exceed 64 KiB"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid Linux capture settings: {}", settings().display()))
}

fn save(key: &str, saved: Saved) -> Result<()> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let mut all = load()?;
    ensure!(
        all.contains_key(key) || all.len() < 32,
        "at most 32 saved portal selections are supported"
    );
    all.insert(key.to_owned(), saved);
    let bytes = serde_json::to_vec(&all)?;
    ensure!(
        bytes.len() <= MAX_SETTINGS as usize,
        "Linux capture settings exceed 64 KiB"
    );
    let path = settings();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .context("create Linux capture settings temporary file")?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub fn displays() -> Result<Vec<Display>> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let mut saved = load()?;
    saved.entry("main".into()).or_default();
    Ok(saved
        .into_iter()
        .filter(|(id, _)| !id.starts_with(WINDOW_KEY))
        .map(|(id, info)| Display {
            name: if id == "main" {
                "Choose a screen (desktop portal)".into()
            } else {
                format!("Portal screen {id}")
            },
            main: id == "main",
            id,
            built_in: false,
            width: info.width,
            height: info.height,
            refresh_rate: 0.0,
        })
        .collect())
}

pub fn allow_prompt() {
    DECLINED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

/// Read the ScreenCast capabilities, never create a session or ask for a grant.
pub fn doctor_check() -> isocast_protocol::api::Check {
    use isocast_protocol::api::{Check, CheckStatus};
    let result = std::thread::spawn(|| -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                let portal =
                    Screencast::with_connection(zbus::Connection::session().await?).await?;
                ensure!(
                    portal
                        .available_source_types()
                        .await?
                        .contains(SourceType::Monitor),
                    "portal doesn't offer screen sharing"
                );
                Ok(())
            })
            .await
            .context("desktop portal did not respond")?
        })
    })
    .join()
    .unwrap_or_else(|_| Err(anyhow::anyhow!("portal check stopped")));
    let fix = result.is_err().then(|| "Install xdg-desktop-portal and your desktop’s screen-sharing backend (e.g. xdg-desktop-portal-gnome or xdg-desktop-portal-kde), then log in to the desktop again.".into());
    Check {
        name: "ScreenCast portal".into(),
        status: if result.is_ok() {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        detail: result.err().map_or_else(
            || "Screen sharing available; the desktop asks when capture starts.".into(),
            |e| e.to_string(),
        ),
        fix,
    }
}

pub struct Capture {
    // Stop PipeWire before closing its portal session.
    video: Option<pipewire_video::Capture>,
    stopping: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Disconnecting PipeWire can itself make the compositor close the
        // session. That is our shutdown, not a revoked grant.
        self.stopping.store(true, Ordering::Release);
        drop(self.video.take());
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Saved window selections are kept under `window:` and the source's id,
/// apart from displays, so each window source keeps its own window.
const WINDOW_KEY: &str = "window:";
/// The longest key a selection is saved under.
const MAX_KEY: usize = 128;

/// What a target asks the portal to share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Screen,
    Window,
}

impl Kind {
    fn source_type(self) -> SourceType {
        match self {
            Kind::Screen => SourceType::Monitor,
            Kind::Window => SourceType::Window,
        }
    }
}

/// The key a target's portal selection is saved under, and what it shares.
fn target_key(target: &ScreenTarget<'_>) -> Result<(String, Kind)> {
    match *target {
        ScreenTarget::Display(id) if !id.is_empty() && !id.starts_with(WINDOW_KEY) => {
            ensure!(
                id.len() <= MAX_KEY,
                "display: a display id must be at most {MAX_KEY} bytes; got {}",
                id.len()
            );
            Ok((id.to_owned(), Kind::Screen))
        }
        ScreenTarget::Window { source, .. } if !source.is_empty() => {
            let key = format!("{WINDOW_KEY}{source}");
            ensure!(
                key.len() <= MAX_KEY,
                "source {source:?}: Linux saves a window source's choice under its id, which \
                 must be at most {} bytes",
                MAX_KEY - WINDOW_KEY.len()
            );
            Ok((key, Kind::Window))
        }
        ScreenTarget::Application { .. } => bail!(
            "application source: Linux's desktop portal shares one screen or one window, never \
             every window of an app; use a window source or a display source instead"
        ),
        _ => bail!(
            "screen target: Linux shares one screen or one window chosen in the desktop portal; \
             give a display source's `display`, or use a window source"
        ),
    }
}

/// Refuse a portal that can't share what the source asks for, instead of
/// showing a dialog with nothing right to choose.
fn check_offered(kind: Kind, offered: BitFlags<SourceType>) -> Result<()> {
    let (wanted, other) = match kind {
        Kind::Screen => ("screens", "a window source"),
        Kind::Window => ("single windows", "a display source"),
    };
    ensure!(
        offered.contains(kind.source_type()),
        "this desktop's screen-sharing portal can't share {wanted} (it offers {offered:?}); use \
         {other} instead"
    );
    Ok(())
}

/// How the dialog was opened: by a source starting, or by `screen_pick`.
#[derive(Clone, Copy)]
enum Asked {
    Start,
    Pick,
}

/// Refuse a stream of the wrong kind. Some desktops' dialogs offer every
/// kind whatever the source asks for (xdg-desktop-portal-hyprland ignores
/// the requested types and opens on its Windows tab), so the error names
/// the tab to use there. A portal that doesn't say what it shared is
/// trusted.
fn check_shared(
    kind: Kind,
    shared: Option<SourceType>,
    asked: Asked,
    hyprland: bool,
) -> Result<()> {
    let again = match asked {
        Asked::Start => "start the source again and choose",
        Asked::Pick => "choose again and pick",
    };
    match (kind, shared) {
        (_, None) | (Kind::Window, Some(SourceType::Window)) => Ok(()),
        (Kind::Screen, Some(SourceType::Monitor | SourceType::Virtual)) => Ok(()),
        (Kind::Window, Some(shared)) => bail!(
            "window source: a {} was chosen in the desktop's sharing dialog, not a window; \
             {again} a window{}, or use a display source to record a screen",
            if shared == SourceType::Virtual {
                "region or virtual screen"
            } else {
                "screen"
            },
            if hyprland { " in its Windows tab" } else { "" }
        ),
        (Kind::Screen, Some(_)) => bail!(
            "display source: a window was chosen in the desktop's sharing dialog, not a screen; \
             {again} a screen{}, or use a window source to record one window",
            if hyprland { " in its Outputs tab" } else { "" }
        ),
    }
}

/// Whether the desktop is Hyprland, whose dialog shows every kind.
fn hyprland() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|desktops| desktops.split(':').any(|desktop| desktop == "Hyprland"))
}

/// The size `target` records at, from the size the portal last gave it,
/// if it was shared before.
pub fn saved_size(target: &ScreenTarget<'_>) -> Option<(u32, u32)> {
    let (key, _) = target_key(target).ok()?;
    let _lock = SETTINGS_LOCK.lock();
    let saved = load().ok()?.remove(&key)?;
    (saved.width > 0 && saved.height > 0).then(|| {
        (
            pipewire_video::even(saved.width),
            pipewire_video::even(saved.height),
        )
    })
}

type Session = ashpd::desktop::Session<Screencast>;

/// The ScreenCast portal on a connection of its own, and a new session.
async fn connect() -> Result<(Screencast, Session)> {
    // A dedicated connection belongs to the caller's runtime. ASHPD's
    // process-global connection would outlive the first runtime.
    tokio::time::timeout(Duration::from_secs(10), async {
        let connection = zbus::Connection::session().await?;
        let portal = Screencast::with_connection(connection)
            .await
            .context("connect to desktop ScreenCast portal")?;
        let session = portal.create_session(Default::default()).await?;
        Ok::<_, anyhow::Error>((portal, session))
    })
    .await
    .context("desktop portal connection timed out")?
}

/// The one stream the portal started for a selection.
struct Selected {
    node: u32,
    width: u32,
    height: u32,
    /// The portal gave a restore token: later starts reopen it silently.
    remembered: bool,
}

/// Ask the portal for one screen or window of `kind`, which `token` may
/// grant without its dialog. A stream of the right kind is saved under
/// `key`; one of the wrong kind is refused and the grant under `key`
/// cleared, so the next start asks again.
async fn select(
    portal: &Screencast,
    session: &Session,
    key: &str,
    kind: Kind,
    asked: Asked,
    cursor: Option<CursorMode>,
    token: Option<&str>,
) -> Result<Selected> {
    check_offered(kind, portal.available_source_types().await?)?;
    let options = SelectSourcesOptions::default()
        .set_sources(Some(kind.source_type().into()))
        .set_multiple(false)
        .set_cursor_mode(cursor)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(token);
    portal.select_sources(session, options).await?.response()?;
    let response = portal
        .start(session, None, Default::default())
        .await?
        .response()?;
    ensure!(
        response.streams().len() == 1,
        "desktop portal must return exactly one screen per source"
    );
    let stream = &response.streams()[0];
    if let Err(error) = check_shared(kind, stream.source_type(), asked, hyprland()) {
        // A start spent the saved token on the wrong kind; forget it so the
        // next start asks. A pick used no token, so the earlier choice stays.
        if let Asked::Start = asked {
            let _ = save(key, Saved::default());
        }
        return Err(error);
    }
    let (width, height) = stream.size().unwrap_or((0, 0));
    let token = response.restore_token().map(str::to_owned);
    let selected = Selected {
        node: stream.pipe_wire_node_id(),
        width: width.max(0) as u32,
        height: height.max(0) as u32,
        remembered: token.is_some(),
    };
    save(
        key,
        Saved {
            token,
            width: selected.width,
            height: selected.height,
        },
    )?;
    Ok(selected)
}

/// Show the desktop's sharing dialog for `target` and keep the choice for
/// it, replacing any it had. Waits up to 3 minutes for the person.
pub fn pick(target: &ScreenTarget<'_>) -> Result<PickedScreen> {
    let (key, kind) = target_key(target)?;
    let thread_key = key.clone();
    let selected = std::thread::Builder::new()
        .name("linux-screen-pick".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (portal, session) = connect().await?;
                let selected = tokio::time::timeout(
                    Duration::from_secs(180),
                    select(
                        &portal,
                        &session,
                        &thread_key,
                        kind,
                        Asked::Pick,
                        None,
                        None,
                    ),
                )
                .await
                .context("nothing was chosen within 3 minutes")
                .and_then(|result| result);
                let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
                selected
            })
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("screen_pick: the portal thread panicked"))?
        .context("screen_pick")?;
    // A choice made now overrides an earlier decline.
    DECLINED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&key);
    // The size it records at: even, or 0 if the portal didn't say.
    let even = |size: u32| {
        if size == 0 {
            0
        } else {
            pipewire_video::even(size)
        }
    };
    Ok(PickedScreen {
        width: even(selected.width),
        height: even(selected.height),
        remembered: selected.remembered,
    })
}

pub fn capture(
    target: ScreenTarget<'_>,
    fps: u32,
    ten_bit: bool,
    cursor: bool,
    sink: VideoSink<Frame>,
) -> Result<(Capture, CaptureFormat)> {
    ensure!(
        !ten_bit,
        "Linux screen capture: 10-bit capture is not supported; select 8-bit ISO recording"
    );
    let (key, kind) = target_key(&target)?;
    {
        let mut declined = DECLINED.lock().unwrap_or_else(|error| error.into_inner());
        declined.retain(|_, when| when.elapsed() < Duration::from_secs(600));
        ensure!(
            !declined.contains_key(&key),
            "screen sharing was declined or stopped; request screen permission to try again, or wait ten minutes"
        );
    }
    let on_stopped = Arc::new(sink.stopped);
    let portal_stopped = on_stopped.clone();
    let sink = VideoSink {
        frame: sink.frame,
        stopped: Box::new(move |reason| on_stopped(reason)),
    };
    let token = {
        let _lock = SETTINGS_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
        load()?.remove(&key).and_then(|saved| saved.token)
    };
    let (send, receive) = mpsc::sync_channel(1);
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let portal_stopping = stopping.clone();
    let thread = std::thread::Builder::new().name("linux-screen-portal".into()).spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async {
                let (portal, session) = tokio::select! {
                    _ = &mut stopped => return Ok(()),
                    result = connect() => result?,
                };
                let mut closed = tokio::time::timeout(Duration::from_secs(5), session.receive_closed())
                    .await.context("desktop portal signal subscription timed out")??;
                let opening = async {
                    let available = portal.available_cursor_modes().await?;
                    let requested = if cursor { CursorMode::Embedded } else { CursorMode::Hidden };
                    ensure!(available.contains(requested), "desktop portal cannot provide the requested cursor mode");
                    let selected = select(&portal, &session, &key, kind, Asked::Start, Some(requested), token.as_deref()).await?;
                    let fd = portal.open_pipe_wire_remote(&session, Default::default()).await?;
                    Ok::<_, anyhow::Error>((fd, selected.node))
                };
                let opened = tokio::select! {
                    _ = &mut stopped => Err(anyhow::anyhow!("screen sharing cancelled")),
                    result = tokio::time::timeout(Duration::from_secs(70), opening) => result
                        .context("screen sharing timed out; approve the desktop portal dialog").and_then(|result| result),
                };
                if let Err(error) = &opened
                    && error.chain().any(|cause| matches!(cause.downcast_ref::<ashpd::Error>(), Some(ashpd::Error::Response(_)))) {
                        let mut declined = DECLINED.lock().unwrap_or_else(|error| error.into_inner());
                        if declined.len() < 32 { declined.insert(key.clone(), Instant::now()); }
                }
                let succeeded = opened.is_ok();
                if send.send(opened).is_ok() && succeeded {
                    tokio::select! {
                        biased;
                        _ = stopped => {},
                        _ = closed.next() => {
                            if !portal_stopping.load(Ordering::Acquire) {
                            let mut declined = DECLINED.lock().unwrap_or_else(|error| error.into_inner());
                            if declined.len() < 32 { declined.insert(key.clone(), Instant::now()); }
                            drop(declined);
                            portal_stopped("desktop portal stopped screen sharing; request screen permission to try again, or wait ten minutes".into());
                            }
                        },
                    }
                }
                let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
                Ok(())
            })
        })();
        if let Err(error) = result { let _ = send.send(Err(error)); }
    })?;
    let mut capture = Capture {
        video: None,
        stopping,
        stop: Some(stop),
        thread: Some(thread),
    };
    // 10s connect + 5s subscription + 70s consent + 15s PipeWire = 100s;
    // even the caller's 90s portal watchdog + 15s fits the 120s open budget.
    let (remote, node) = receive
        .recv_timeout(Duration::from_secs(90))
        .context("desktop portal did not respond")??;
    let (video, format) = pipewire_video::start(remote, node, fps, sink)?;
    capture.video = Some(video);
    Ok((capture, format))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(source: &str) -> ScreenTarget<'_> {
        ScreenTarget::Window {
            app: "firefox",
            title: None,
            source,
        }
    }

    #[test]
    fn each_window_source_keeps_its_own_grant_apart_from_displays() {
        assert_eq!(
            target_key(&ScreenTarget::Display("main")).unwrap(),
            ("main".into(), Kind::Screen)
        );
        assert_eq!(
            target_key(&window("slides")).unwrap(),
            ("window:slides".into(), Kind::Window)
        );
        // Two sources of one app, matched by the same app, keep two grants.
        assert_ne!(
            target_key(&window("slides")).unwrap(),
            target_key(&window("notes")).unwrap()
        );
        for refused in [
            ScreenTarget::Display(""),
            ScreenTarget::Display("window:slides"),
            window(""),
        ] {
            assert!(target_key(&refused).is_err());
        }
        let longest = "x".repeat(MAX_KEY - WINDOW_KEY.len());
        assert!(target_key(&window(&longest)).is_ok());
        let error = target_key(&window(&format!("{longest}x")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("which must be at most 121 bytes"), "{error}");
        let display = "x".repeat(MAX_KEY + 1);
        let error = target_key(&ScreenTarget::Display(&display))
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("display: a display id must be at most 128 bytes"),
            "{error}"
        );
    }

    #[test]
    fn application_sources_suggest_a_window_or_display() {
        let error = target_key(&ScreenTarget::Application {
            app: "firefox",
            display: "main",
        })
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("application source:"), "{error}");
        assert!(
            error.contains("use a window source or a display source"),
            "{error}"
        );
    }

    #[test]
    fn a_portal_that_cannot_share_the_kind_is_refused_before_its_dialog() {
        let monitors = BitFlags::from(SourceType::Monitor);
        check_offered(Kind::Screen, monitors).unwrap();
        let error = check_offered(Kind::Window, monitors)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("this desktop's screen-sharing portal can't share single windows"),
            "{error}"
        );
        assert!(error.ends_with("use a display source instead"), "{error}");
        check_offered(Kind::Window, SourceType::Monitor | SourceType::Window).unwrap();
    }

    #[test]
    fn the_wrong_kind_chosen_in_the_dialog_is_refused() {
        for (kind, shared) in [
            (Kind::Window, Some(SourceType::Window)),
            (Kind::Window, None),
            (Kind::Screen, Some(SourceType::Monitor)),
            (Kind::Screen, Some(SourceType::Virtual)),
            (Kind::Screen, None),
        ] {
            check_shared(kind, shared, Asked::Start, true).unwrap();
        }
        let refused = |kind, shared, asked, hyprland| {
            check_shared(kind, Some(shared), asked, hyprland)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            refused(Kind::Screen, SourceType::Window, Asked::Pick, true),
            "display source: a window was chosen in the desktop's sharing dialog, not a screen; \
             choose again and pick a screen in its Outputs tab, or use a window source to record \
             one window"
        );
        assert_eq!(
            refused(Kind::Window, SourceType::Monitor, Asked::Start, false),
            "window source: a screen was chosen in the desktop's sharing dialog, not a window; \
             start the source again and choose a window, or use a display source to record a \
             screen"
        );
        assert!(
            refused(Kind::Window, SourceType::Virtual, Asked::Pick, true).starts_with(
                "window source: a region or virtual screen was chosen in the desktop's sharing \
                 dialog, not a window; choose again and pick a window in its Windows tab"
            )
        );
    }
}
