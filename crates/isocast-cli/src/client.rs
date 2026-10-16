//! Talking to the daemon (the shared `isocast-client` crate), and the CLI's
//! and MCP's helpers on top of it.

use anyhow::{Result, bail};
use isocast_protocol::api::{Applied, ApplyParams, Live, Method};
use isocast_protocol::change::Change;

pub use isocast_client::Client;

/// Change audio source `source`'s program mix settings in the live setup
/// (a running recording's mix follows gain and mute at once): a merge patch
/// applied at the live revision.
/// A change to an audio source's mix settings; absent fields stay as they
/// are.
#[derive(Debug, Default, serde::Serialize)]
pub struct AudioChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<isocast_protocol::Monitoring>,
}

pub fn set_audio(client: &mut Client, source: &str, change: &AudioChange) -> Result<Applied> {
    let live: Live = client.call(Method::Get(None))?;
    let Some(setup) = live.setup else {
        bail!("there is no live setup; apply one first");
    };
    let audio: Vec<&str> = setup
        .sources
        .iter()
        .filter(|s| s.kind.has_audio())
        .map(|s| s.id.as_str())
        .collect();
    if !audio.contains(&source) {
        bail!(
            "no audio source {source:?}; audio sources: {}",
            audio.join(", ")
        );
    }
    let settings = serde_json::to_value(change)?;
    if settings.as_object().is_none_or(|fields| fields.is_empty()) {
        bail!(
            "nothing to change: give mono/stereo, a gain, mute or unmute, an offset, a balance or a \
             monitoring type"
        );
    }
    let patch = serde_json::json!({ "sources": [{ "id": source, "audio": settings }] });
    client.call(Method::Apply(Box::new(ApplyParams {
        revision: live.revision,
        change: Change::Patch(patch),
        live: false,
    })))
}

/// Warn on stderr when the engine that answered (`status`'s `daemon`) is
/// older than the one this distribution bundles: left running from before
/// an update, it would otherwise keep serving this newer isocast silently.
/// Restarting it is left to the person (`isocast restart`, refused while
/// it records or streams).
pub fn warn_if_stale(daemon: Option<&isocast_protocol::api::DaemonBuild>) {
    if let Some(stale) = isocast_client::stale_engine(daemon) {
        let pid = stale
            .pid
            .map(|pid| format!(", pid {pid}"))
            .unwrap_or_default();
        isocast_diagnostics::diagnostic!("warn";
            "warning: the running isocastd {}{pid} is older than this isocast (build {}); \
             run `isocast restart` once nothing is recording or streaming to use this version's \
             engine",
            stale.running, stale.expected);
    }
}

/// [`warn_if_stale`] for a command that doesn't otherwise read `status`:
/// asks for it only in a distribution, where there is a bundled engine to
/// compare with.
pub fn check_engine(client: &mut Client) {
    if isocast_client::bundled_build().is_none() {
        return;
    }
    if let Ok(status) = client.call::<isocast_protocol::api::Status>(Method::Status) {
        warn_if_stale(status.daemon.as_ref());
    }
}

/// Where an ISO file's audio tracks come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IsoAudioFrom {
    /// Its own list, `recording.iso.source_audio.<source>`.
    SourceAudio,
    /// The shared `recording.iso.audio`.
    Shared,
    /// Neither is set: the source's own sound.
    Own,
}

/// One ISO file's audio tracks, as the live setup chooses them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IsoAudio {
    pub source: String,
    /// Whether the file is recorded (`recording.iso.sources` lists it).
    pub recorded: bool,
    /// Its audio tracks in order, if the source has the sound its kind
    /// can have (a media file without audio adds no own track).
    pub audio: Vec<String>,
    pub from: IsoAudioFrom,
}

/// Every video source's ISO tracks in `setup`, ISO sources first.
pub fn iso_audio(setup: &isocast_protocol::Setup) -> Vec<IsoAudio> {
    let iso = &setup.recording.iso;
    let mut rows: Vec<IsoAudio> = setup
        .sources
        .iter()
        .filter(|s| s.kind.has_video())
        .map(|source| IsoAudio {
            source: source.id.clone(),
            recorded: iso.sources.contains(&source.id),
            audio: iso.audio_for(source, source.kind.has_audio()),
            from: match (iso.source_audio.contains_key(&source.id), &iso.audio) {
                (true, _) => IsoAudioFrom::SourceAudio,
                (false, Some(_)) => IsoAudioFrom::Shared,
                (false, None) => IsoAudioFrom::Own,
            },
        })
        .collect();
    rows.sort_by_key(|row| !row.recorded);
    rows
}

/// Choose the audio tracks of video source `source`'s ISO file in the live
/// setup: `Some(ids)` exactly those (`[]`: no audio), `None` back to
/// `recording.iso.audio`'s choice. A merge patch at the live revision.
pub fn set_iso_audio(
    client: &mut Client,
    source: &str,
    audio: Option<&[String]>,
) -> Result<Applied> {
    let live: Live = client.call(Method::Get(None))?;
    let Some(setup) = live.setup else {
        bail!("there is no live setup; apply one first");
    };
    let video: Vec<&str> = setup
        .sources
        .iter()
        .filter(|s| s.kind.has_video())
        .map(|s| s.id.as_str())
        .collect();
    if !video.contains(&source) {
        bail!(
            "source: no video source {source:?}; video sources: {}",
            video.join(", ")
        );
    }
    let patch = serde_json::json!({
        "recording": { "iso": { "source_audio": { source: audio } } }
    });
    client.call(Method::Apply(Box::new(ApplyParams {
        revision: live.revision,
        change: Change::Patch(patch),
        live: false,
    })))
}

/// Stop isocastd and start it again (after permissions change, it only
/// sees them once restarted). The daemon refuses, atomically, while it
/// records, streams, exports or feeds the virtual camera
/// (`shutdown_if_idle`), so a start racing the restart is never cut short.
pub fn restart() -> Result<()> {
    if let Some(mut client) = Client::connect()? {
        let _: serde_json::Value = client.call(Method::ShutdownIfIdle)?;
        // Wait for the old daemon to let go of the socket.
        for _ in 0..100 {
            if Client::connect()?.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Client::connect_or_launch()?;
    Ok(())
}
