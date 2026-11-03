//! A single user-selected portal stream per display or window source. The
//! portal owns consent and cursor compositing; discovery never opens a
//! share dialog.
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use ashpd::enumflags2::BitFlags;
use futures_util::StreamExt;
use isocast_engine::{CaptureFormat, NotChosen, PickedScreen, ScreenTarget, VideoSink};
use isocast_protocol::devices::Display;
use serde::{Deserialize, Serialize};

use crate::{frames::Frame, pipewire_video};

static SETTINGS_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
/// Held while a source start or a pick talks to the portal, so at most one
/// sharing dialog is open at a time: the dialog doesn't say which source
/// it is for, and the person answers them one after another.
static DIALOG: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MAX_SETTINGS: u64 = 64 * 1024;
/// The most choices kept: with keys at their longest, they fit in
/// `MAX_SETTINGS`.
const MAX_SAVED: usize = 128;

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    token: Option<String>,
    width: u32,
    height: u32,
}

fn settings() -> std::path::PathBuf {
    isocast_protocol::api::setup_path().with_extension("linux-capture.json")
}

fn load(path: &Path) -> Result<BTreeMap<String, Saved>> {
    let file = match std::fs::File::open(path) {
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
        .with_context(|| format!("invalid Linux capture settings: {}", path.display()))
}

fn save(key: &str, saved: Saved) -> Result<()> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let path = settings();
    let mut all = load(&path)?;
    check_room(&all, key)?;
    all.insert(key.to_owned(), saved);
    write(&path, &all)
}

/// Refuse a new choice once `MAX_SAVED` are kept.
fn check_room(all: &BTreeMap<String, Saved>, key: &str) -> Result<()> {
    ensure!(
        all.contains_key(key) || all.len() < MAX_SAVED,
        "Linux keeps at most {MAX_SAVED} screen and window choices, and has that many; remove \
         window sources you no longer use, in any scene collection"
    );
    Ok(())
}

/// Forget the windows chosen for window sources not in `sources`.
pub fn keep_windows(sources: &std::collections::BTreeSet<String>) -> Result<()> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let path = settings();
    let mut all = load(&path)?;
    let before = all.len();
    all.retain(|key, _| {
        key.strip_prefix(WINDOW_KEY)
            .is_none_or(|source| sources.contains(source))
    });
    if all.len() != before {
        write(&path, &all)?;
    }
    Ok(())
}

/// Forget the choice saved under `key`, so its source waits for a pick.
fn forget(path: &Path, key: &str) -> Result<()> {
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let mut all = load(path)?;
    if all.remove(key).is_some() {
        write(path, &all)?;
    }
    Ok(())
}

fn write(path: &Path, all: &BTreeMap<String, Saved>) -> Result<()> {
    let bytes = serde_json::to_vec(all)?;
    ensure!(
        bytes.len() <= MAX_SETTINGS as usize,
        "Linux capture settings exceed 64 KiB"
    );
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
        std::fs::rename(&tmp, path)?;
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
    let mut saved = load(&settings())?;
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

/// Called once if the portal closes a share after it started.
type OnClosed = Box<dyn FnOnce() + Send>;

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
            || {
                "Screen sharing available; choose each screen or window with `isocast screen pick`."
                    .into()
            },
            |e| e.to_string(),
        ),
        fix,
    }
}

/// One open portal share: its PipeWire remote and what was chosen, and
/// the thread that keeps its session open until the share is dropped or
/// the portal closes it.
struct Share {
    /// Taken by the capture that starts PipeWire on it.
    remote: Option<OwnedFd>,
    selected: Selected,
    /// Whether it shows the cursor (a pick hands a share only to a start
    /// that asks the same).
    cursor: bool,
    on_closed: Arc<Mutex<Option<OnClosed>>>,
    /// The portal closed it.
    closed: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Share {
    fn drop(&mut self) {
        // Closing our own session is our shutdown, not a revoked grant.
        self.stopping.store(true, Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A running screen or window capture. Its fields drop in order: PipeWire
/// stops before the portal session closes.
pub struct Capture {
    _video: pipewire_video::Capture,
    share: Share,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Disconnecting PipeWire can itself make the compositor close the
        // session. That is our shutdown, not a revoked grant.
        self.share.stopping.store(true, Ordering::Release);
    }
}

/// Shares `screen_pick` opened, each kept a few seconds (with a number
/// telling them apart) for its source to take over when it reopens, so the
/// person chooses once even where the desktop keeps no restore token.
static HANDOFF: Mutex<BTreeMap<String, (u64, Share)>> = Mutex::new(BTreeMap::new());
const HANDOFF_FOR: Duration = Duration::from_secs(15);
static NEXT_HANDOFF: AtomicU64 = AtomicU64::new(0);

fn handoff() -> std::sync::MutexGuard<'static, BTreeMap<String, (u64, Share)>> {
    HANDOFF.lock().unwrap_or_else(|error| error.into_inner())
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

    fn noun(self) -> &'static str {
        match self {
            Kind::Screen => "screen",
            Kind::Window => "window",
        }
    }
}

/// The key a target's portal selection is saved under, and what it shares.
fn target_key(target: &ScreenTarget<'_>) -> Result<(String, Kind)> {
    match *target {
        ScreenTarget::Display { display: id, .. }
            if !id.is_empty() && !id.starts_with(WINDOW_KEY) =>
        {
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
/// the requested types and shows every tab), so the error names the tab to
/// use there. A portal that doesn't say what it shared is trusted.
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
    let saved = load(&settings()).ok()?.remove(&key)?;
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
        // source waits for a pick. A pick used no token, so the earlier
        // choice stays.
        if let Asked::Start = asked {
            let _ = forget(&settings(), key);
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

/// A restore token is single-use. Read it only after any earlier dialog
/// has finished spending it and saving its replacement.
async fn begin_dialog(
    path: &Path,
    key: &str,
    kind: Kind,
    asked: Asked,
) -> Result<(tokio::sync::MutexGuard<'static, ()>, Option<String>)> {
    let guard = tokio::time::timeout(Duration::from_secs(10), DIALOG.lock())
        .await
        .context("another source's sharing dialog is open; answer it first")?;
    let _lock = SETTINGS_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("capture settings lock poisoned"))?;
    let mut all = load(path)?;
    let token = match asked {
        Asked::Start => Some(
            all.remove(key)
                .and_then(|saved| saved.token)
                .ok_or(NotChosen { what: kind.noun() })?,
        ),
        Asked::Pick => {
            check_room(&all, key)?;
            None
        }
    };
    Ok((guard, token))
}

/// Open a portal share for `key`. A start restores its token where the
/// portal can, else the portal shows its sharing dialog; a pick always
/// shows it. Waits for the person up to 70 s for a start, 3 minutes for a
/// pick; fails if another dialog of ours stays open past 10 s.
fn open(key: String, kind: Kind, asked: Asked, cursor: bool) -> Result<Share> {
    let answer = Duration::from_secs(match asked {
        Asked::Start => 70,
        Asked::Pick => 180,
    });
    let on_closed: Arc<Mutex<Option<OnClosed>>> = Arc::default();
    let closed = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let (thread_closed, thread_on_closed, thread_stopping) =
        (closed.clone(), on_closed.clone(), stopping.clone());
    let (send, receive) = mpsc::sync_channel(1);
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new().name("linux-screen-portal".into()).spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async {
                let path = settings();
                let (dialog, token) = tokio::select! {
                    _ = &mut stopped => return Ok(()),
                    result = begin_dialog(&path, &key, kind, asked) => result?,
                };
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
                    let selected = select(&portal, &session, &key, kind, asked, Some(requested), token.as_deref()).await?;
                    let fd = portal.open_pipe_wire_remote(&session, Default::default()).await?;
                    Ok::<_, anyhow::Error>((fd, selected))
                };
                let opened = tokio::select! {
                    _ = &mut stopped => Err(anyhow::anyhow!("screen sharing cancelled")),
                    result = tokio::time::timeout(answer, opening) => result
                        .with_context(|| format!(
                            "nothing was chosen in the desktop's sharing dialog within {} s",
                            answer.as_secs()
                        ))
                        .and_then(|result| result),
                };
                // The portal couldn't restore a start's saved choice, and its
                // dialog was cancelled or left: forget the spent token, so the
                // source waits for a pick instead of showing the dialog again.
                // A pick used no token, so the earlier choice stays.
                if let Asked::Start = asked
                    && let Err(error) = &opened
                    && (error.is::<tokio::time::error::Elapsed>() || error.chain().any(|cause| matches!(cause.downcast_ref::<ashpd::Error>(), Some(ashpd::Error::Response(_))))) {
                        let _ = forget(&path, &key);
                }
                drop(dialog);
                let succeeded = opened.is_ok();
                if send.send(opened).is_ok() && succeeded {
                    tokio::select! {
                        biased;
                        _ = stopped => {},
                        _ = closed.next() => {
                            if !thread_stopping.load(Ordering::Acquire) {
                                end_share(&path, &key, &thread_closed);
                                let on_closed = thread_on_closed.lock().unwrap_or_else(|error| error.into_inner()).take();
                                if let Some(on_closed) = on_closed {
                                    on_closed();
                                }
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
    // 10 s connect + 5 s subscription + 10 s for another dialog + the answer,
    // and 5 s to spare. For a start that is 100 s; with 15 s for PipeWire it
    // fits the 120 s open budget.
    let (remote, selected) = match receive.recv_timeout(answer + Duration::from_secs(30)) {
        Ok(Ok(opened)) => opened,
        result => {
            stopping.store(true, Ordering::Release);
            let _ = stop.send(());
            let _ = thread.join();
            return Err(match result {
                Ok(Err(error)) => error,
                _ => anyhow::anyhow!("desktop portal did not respond"),
            });
        }
    };
    Ok(Share {
        remote: Some(remote),
        selected,
        cursor,
        on_closed,
        closed,
        stopping,
        stop: Some(stop),
        thread: Some(thread),
    })
}

/// Show the desktop's sharing dialog for `target` and keep the choice for
/// it, replacing any it had. Waits up to 3 minutes for the person. The
/// share stays open for a few seconds, so the source reopening on it
/// doesn't ask again.
pub fn pick(target: &ScreenTarget<'_>, cursor: bool) -> Result<PickedScreen> {
    let (key, kind) = target_key(target)?;
    let share = open(key.clone(), kind, Asked::Pick, cursor).context("screen_pick")?;
    // The size it records at: even, or 0 if the portal didn't say.
    let even = |size: u32| {
        if size == 0 {
            0
        } else {
            pipewire_video::even(size)
        }
    };
    let picked = PickedScreen {
        width: even(share.selected.width),
        height: even(share.selected.height),
        remembered: share.selected.remembered,
    };
    let number = NEXT_HANDOFF.fetch_add(1, Ordering::Relaxed);
    // The expiry starts first: a share is never kept without one.
    let expiring = key.clone();
    std::thread::Builder::new()
        .name("linux-screen-handoff".into())
        .spawn(move || {
            std::thread::sleep(HANDOFF_FOR);
            let mut shares = handoff();
            let expired = match shares.get(&expiring) {
                Some((kept, _)) if *kept == number => shares.remove(&expiring),
                _ => None,
            };
            // Drop the share after unlocking: its Drop joins the portal thread.
            drop(shares);
            drop(expired);
        })?;
    let replaced = handoff().insert(key, (number, share));
    drop(replaced);
    Ok(picked)
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
    // A share picked for this key, if it shows the cursor as this source
    // does; another source on the same display may take one that doesn't.
    let picked = {
        let mut shares = handoff();
        match shares.get(&key) {
            Some((_, share)) if share.cursor == cursor => shares.remove(&key),
            _ => None,
        }
    };
    let mut share = match picked {
        Some((_, share)) => share,
        None => open(key.clone(), kind, Asked::Start, cursor)?,
    };
    let on_stopped: Arc<dyn Fn(String) + Send + Sync> = Arc::from(sink.stopped);
    let portal_stopped = on_stopped.clone();
    let reason = closed_reason(&target);
    *share
        .on_closed
        .lock()
        .unwrap_or_else(|error| error.into_inner()) =
        Some(Box::new(move || portal_stopped(reason)));
    ensure!(
        !share.closed.load(Ordering::Acquire),
        "{}",
        closed_reason(&target)
    );
    let remote = share
        .remote
        .take()
        .context("desktop portal share has no remote")?;
    let closed = share.closed.clone();
    let reason = closed_reason(&target);
    let (video, format) = pipewire_video::start(
        remote,
        share.selected.node,
        fps,
        move || {
            end_share(&settings(), &key, &closed);
            reason
        },
        VideoSink {
            frame: sink.frame,
            stopped: Box::new(move |reason| on_stopped(reason)),
        },
    )?;
    Ok((
        Capture {
            _video: video,
            share,
        },
        format,
    ))
}

/// Both `Closed` and node removal can arrive for the same share. Never
/// erase a later pick on the second notification.
fn end_share(path: &Path, key: &str, closed: &AtomicBool) {
    if !closed.swap(true, Ordering::AcqRel)
        && let Err(error) = forget(path, key)
    {
        isocast_diagnostics::diagnostic!("warn"; "could not forget the ended share {key:?}: {error:#}");
    }
}

/// The stop reason when a portal share or its PipeWire node goes away.
fn closed_reason(target: &ScreenTarget<'_>) -> String {
    match *target {
        ScreenTarget::Window { source, .. } => format!(
            "the window shared for source {source:?} was closed, or its sharing was stopped; \
             choose a window with `isocast screen pick {source}` (method `screen_pick`)"
        ),
        ScreenTarget::Display { display, source } => format!(
            "sharing display {display:?} was stopped; choose a screen with `isocast screen pick {source}` (method `screen_pick`)"
        ),
        _ => "screen sharing was stopped".into(),
    }
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

    fn display(id: &str) -> ScreenTarget<'_> {
        ScreenTarget::Display {
            display: id,
            source: "slides",
        }
    }

    #[test]
    fn each_window_source_keeps_its_own_grant_apart_from_displays() {
        assert_eq!(
            target_key(&display("main")).unwrap(),
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
        for refused in [display(""), display("window:slides"), window("")] {
            assert!(target_key(&refused).is_err());
        }
        let longest = "x".repeat(MAX_KEY - WINDOW_KEY.len());
        assert!(target_key(&window(&longest)).is_ok());
        let error = target_key(&window(&format!("{longest}x")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("which must be at most 121 bytes"), "{error}");
        let long = "x".repeat(MAX_KEY + 1);
        let error = target_key(&display(&long)).unwrap_err().to_string();
        assert!(
            error.starts_with("display: a display id must be at most 128 bytes"),
            "{error}"
        );
    }

    #[test]
    fn a_full_store_refuses_new_choices_but_keeps_replacing_old_ones() {
        let mut all: BTreeMap<String, Saved> = (0..MAX_SAVED)
            .map(|n| (format!("{WINDOW_KEY}{n}"), Saved::default()))
            .collect();
        check_room(&all, "window:0").unwrap();
        let error = check_room(&all, "window:new").unwrap_err().to_string();
        assert!(
            error.starts_with("Linux keeps at most 128 screen and window choices"),
            "{error}"
        );
        all.remove("window:0");
        check_room(&all, "window:new").unwrap();
        // At their longest, that many still fit the settings file.
        let longest: BTreeMap<String, Saved> = (0..MAX_SAVED)
            .map(|n| {
                let key = format!("{n:0>width$}", width = MAX_KEY);
                let saved = Saved {
                    token: Some("f".repeat(64)),
                    width: u32::MAX,
                    height: u32::MAX,
                };
                (key, saved)
            })
            .collect();
        assert!(serde_json::to_vec(&longest).unwrap().len() <= MAX_SETTINGS as usize);
    }

    #[test]
    fn a_closed_share_names_the_pick_that_chooses_again() {
        assert_eq!(
            closed_reason(&window("terminal")),
            "the window shared for source \"terminal\" was closed, or its sharing was stopped; \
             choose a window with `isocast screen pick terminal` (method `screen_pick`)"
        );
        assert_eq!(
            closed_reason(&display("main")),
            "sharing display \"main\" was stopped; choose a screen with `isocast screen pick slides` (method `screen_pick`)"
        );
        let not_chosen = NotChosen {
            what: Kind::Window.noun(),
        };
        assert_eq!(not_chosen.to_string(), "no window is chosen for it yet");
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

    fn saved_choices() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "isocast-portal-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("choices.json");
        let choices = ["window:terminal", "main"].map(|key| {
            (
                key.to_owned(),
                Saved {
                    token: Some("first".into()),
                    ..Default::default()
                },
            )
        });
        write(&path, &choices.into()).unwrap();
        path
    }

    #[tokio::test]
    async fn an_ended_node_forgets_only_its_share_and_retry_needs_a_pick() {
        let path = saved_choices();
        let (key, kind) = target_key(&window("terminal")).unwrap();
        let closed = AtomicBool::new(false);
        end_share(&path, &key, &closed);
        let err = begin_dialog(&path, &key, kind, Asked::Start)
            .await
            .unwrap_err();
        assert_eq!(err.downcast_ref::<NotChosen>().unwrap().what, "window");
        let mut choices = load(&path).unwrap();
        assert!(choices.contains_key("main"));
        // A late Closed for the same share must not forget a fresh pick.
        choices.insert(key.clone(), Saved::default());
        write(&path, &choices).unwrap();
        end_share(&path, &key, &closed);
        assert!(load(&path).unwrap().contains_key(&key));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn two_starts_on_one_display_use_successive_restore_tokens() {
        let path = saved_choices();
        let start = || begin_dialog(&path, "main", Kind::Screen, Asked::Start);
        let (first, token) = start().await.unwrap();
        assert_eq!(token.as_deref(), Some("first"));
        let mut second = Box::pin(start());
        assert!(futures_util::poll!(&mut second).is_pending());
        let mut choices = load(&path).unwrap();
        choices.get_mut("main").unwrap().token = Some("renewed".into());
        write(&path, &choices).unwrap();
        drop(first);
        let (_second, token) = second.await.unwrap();
        assert_eq!(token.as_deref(), Some("renewed"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
