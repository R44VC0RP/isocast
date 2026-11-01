//! A single user-selected portal stream per display source. The portal owns
//! consent and cursor compositing; discovery never opens a share dialog.
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
use futures_util::StreamExt;
use isocast_engine::{CaptureFormat, ScreenTarget, VideoSink};
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
    let key = match target {
        ScreenTarget::Display(id) if !id.is_empty() && id.len() <= 128 => id.to_owned(),
        _ => bail!(
            "screen target: Linux supports display sources selected through the desktop portal"
        ),
    };
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
                // A dedicated connection belongs to this runtime. ASHPD's
                // process-global connection would outlive the first runtime.
                let connecting = tokio::time::timeout(Duration::from_secs(10), async {
                    let connection = zbus::Connection::session().await?;
                    let portal = Screencast::with_connection(connection).await.context("connect to desktop ScreenCast portal")?;
                    let session = portal.create_session(Default::default()).await?;
                    Ok::<_, anyhow::Error>((portal, session))
                });
                let (portal, session) = tokio::select! {
                    _ = &mut stopped => return Ok(()),
                    result = connecting => result.context("desktop portal connection timed out")??,
                };
                let mut closed = tokio::time::timeout(Duration::from_secs(5), session.receive_closed())
                    .await.context("desktop portal signal subscription timed out")??;
                let opening = async {
                    let available = portal.available_cursor_modes().await?;
                    let requested = if cursor { CursorMode::Embedded } else { CursorMode::Hidden };
                    ensure!(available.contains(requested), "desktop portal cannot provide the requested cursor mode");
                    let options = SelectSourcesOptions::default()
                        .set_sources(Some(SourceType::Monitor.into())).set_multiple(false)
                        .set_cursor_mode(requested).set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(token.as_deref());
                    portal.select_sources(&session, options).await?.response()?;
                    let response = portal.start(&session, None, Default::default()).await?.response()?;
                    ensure!(response.streams().len() == 1, "desktop portal must return exactly one screen per source");
                    let stream = &response.streams()[0];
                    let (width, height) = stream.size().unwrap_or((0, 0));
                    save(&key, Saved { token: response.restore_token().map(str::to_owned), width: width.max(0) as u32, height: height.max(0) as u32 })?;
                    let fd = portal.open_pipe_wire_remote(&session, Default::default()).await?;
                    Ok::<_, anyhow::Error>((fd, stream.pipe_wire_node_id()))
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
