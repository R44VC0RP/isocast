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
