//! First launch and Tools › Auto-Configuration Wizard (#46): `obs_discover`
//! (OBS's collections and profiles on this Mac), `speed_test` (the upload
//! to a streaming service) and `optimize` (the live setup for recording,
//! streaming or both, checked by `tune`). The rules live in
//! `isocast_protocol::optimize`.

use std::path::PathBuf;

use isocast_engine::stream::speed;
use isocast_protocol::api::{ApplyParams, RpcError, TuneParams};
use isocast_protocol::change::{Change, diff, merge_patch};
use isocast_protocol::obs::{self, Discovery};
use isocast_protocol::optimize::{
    self, ObsDiscoverParams, OptimizeParams, Optimized, SPEED_TEST_CAP_KBPS, SpeedTest,
    SpeedTestParams, StreamChoice,
};
use isocast_protocol::stream::{STREAM_SERVICES, StreamService, StreamSettings};
use isocast_protocol::{Setup, stream::key_account};

use crate::{Daemon, KEY_ENV, key_service, stream_key, stream_settings};

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    }
}

fn wrong_state(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

/// Services that have no test mode: a test publish would go out live.
fn no_test_mode(service: StreamService) -> Option<&'static str> {
    match service {
        StreamService::Youtube => Some("YouTube"),
        StreamService::Facebook => Some("Facebook"),
        StreamService::Kick => Some("Kick"),
        StreamService::Twitch | StreamService::Custom => None,
    }
}

impl Daemon {
    pub(crate) fn obs_discover(
        &self,
        params: Option<ObsDiscoverParams>,
    ) -> Result<Discovery, RpcError> {
        let folder = match params.and_then(|p| p.folder) {
            Some(folder) if !folder.trim().is_empty() => PathBuf::from(folder),
            _ => {
                let home = std::env::var_os("HOME").map(PathBuf::from);
                match (std::env::var_os(obs::FOLDER_ENV), home) {
                    (Some(folder), _) if !folder.is_empty() => PathBuf::from(folder),
                    (_, Some(home)) => obs::obs_folder(&home),
                    _ => {
                        return Err(failed(
                            "folder: isocastd has no HOME, so give OBS's settings folder",
                        ));
                    }
                }
            }
        };
        Ok(obs::discover(&folder))
    }

    pub(crate) fn speed_test(&self, params: SpeedTestParams) -> Result<SpeedTest, RpcError> {
        let state = self.state.lock().unwrap();
        if state.streaming() {
            return Err(wrong_state(
                "not while streaming: the test would compete with the stream for the upload",
            ));
        }
        let live = stream_settings(&state);
        // The bitrate is for the canvas's rate, as optimize picks it.
        let fps = state.setup.as_ref().map_or(60, |s| s.canvas.fps);
        drop(state);
        let service = match (params.service, &live) {
            (Some(service), _) => service,
            (None, Some(live)) => live.service,
            (None, None) => {
                let ids: Vec<&str> = STREAM_SERVICES.iter().map(|s| s.id()).collect();
                return Err(failed(format!(
                    "service: which service? Give one ({}), or add a [stream] section to the \
                     live setup",
                    ids.join(", ")
                )));
            }
        };
        let server = params.server.clone().or_else(|| {
            live.as_ref()
                .filter(|l| l.service == service)
                .and_then(|l| l.server.clone())
        });
        let seconds = params.seconds.unwrap_or(8);
        if !(3..=20).contains(&seconds) {
            return Err(failed("seconds: from 3 to 20"));
        }
        let settings: StreamSettings = serde_json::from_value(serde_json::json!({
            "service": service.id(),
            "server": server,
        }))
        .map_err(|err| failed(format!("server: {err}")))?;
        let url = settings.server().map_err(failed)?;
        // Fallbacks: a stream key the test can use, and a service that
        // doesn't put the test on air.
        let fallback = |reason: String| SpeedTest {
            service,
            server: Some(url.to_string()),
            measured: false,
            upload_kbps: None,
            method: None,
            capped: false,
            seconds: 0.0,
            bytes: 0,
            recommended_kbps: optimize::stream_bitrate(service, None, fps),
            reason: Some(reason),
        };
        if let Some(name) = no_test_mode(service) {
            return Ok(fallback(format!(
                "{name} has no test mode, so a test would go out live; the stream uses a safe \
                 {} instead (raise it in Settings › Stream if your connection allows)",
                optimize::mbps(
                    optimize::UNMEASURED_KBPS.min(optimize::service_ceiling(service, fps))
                )
            )));
        }
        let account = key_account(service, settings.server.as_deref());
        let key = match std::env::var(KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => Some(key.trim().to_string()),
            _ => stream_key(&account).map_err(crate::failed)?,
        };
        let Some(mut key) = key else {
            return Ok(fallback(format!(
                "no stream key is set for {}, so the upload couldn't be tested; set it (Settings \
                 › Stream, or `isocast stream key set`) and test again",
                service.id()
            )));
        };
        // Errors never show the key, whichever form the server echoes.
        let bare = key.clone();
        isocast_diagnostics::redact::remember_secret(key_service(), &account, &bare);
        // Twitch takes it without going live.
        if service == StreamService::Twitch {
            key.push_str("?bandwidthtest=true");
        }
        let measured = speed::upload_test(
            &url,
            &key,
            f64::from(seconds),
            SPEED_TEST_CAP_KBPS,
            &self.shutting_down,
        )
        .map_err(|err| {
            failed(format!(
                "the upload test to {url} failed: {}",
                isocast_engine::stream::redact(
                    &isocast_engine::stream::redact(&format!("{err:#}"), &key),
                    &bare
                )
            ))
        })?;
        Ok(SpeedTest {
            service,
            server: Some(url.to_string()),
            measured: true,
            upload_kbps: Some(measured.kbps),
            method: Some(measured.method.to_string()),
            capped: measured.capped,
            seconds: measured.seconds,
            bytes: measured.bytes,
            recommended_kbps: optimize::stream_bitrate(service, Some(measured.kbps), fps),
            reason: None,
        })
    }

    pub(crate) fn optimize(&self, params: OptimizeParams) -> Result<Optimized, RpcError> {
        let state = self.state.lock().unwrap();
        if state.session.is_some() || state.streaming() {
            return Err(wrong_state(
                "not while recording or streaming: the benchmark would compete for the \
                 encoders; stop first",
            ));
        }
        let Some(live) = state.setup.clone() else {
            return Err(state.no_setup("no setup to optimize; make one first"));
        };
        // `apply` refuses the result if the setup changed meanwhile (the
        // benchmark takes a minute).
        let revision = state.revision;
        drop(state);
        let mut notes = Vec::new();
        let mut speed = None;
        let choice = if params.goal.streams() {
            let service = match (params.service, &live.stream) {
                (Some(service), _) => service,
                (None, Some(stream)) => stream.service,
                (None, None) => {
                    let ids: Vec<&str> = STREAM_SERVICES.iter().map(|s| s.id()).collect();
                    return Err(failed(format!(
                        "service: streaming needs a service ({})",
                        ids.join(", ")
                    )));
                }
            };
            let server = params.server.clone().or_else(|| {
                live.stream
                    .as_ref()
                    .filter(|s| s.service == service)
                    .and_then(|s| s.server.clone())
            });
            let upload_kbps = match params.upload_kbps {
                Some(kbps) => Some(kbps),
                // A custom server (maybe a relay to a live service) is
                // only tested when asked (`speed_test`).
                None if service == StreamService::Custom => {
                    notes.push(
                        "a custom server's upload is only tested when asked (`isocast stream \
                         speed-test`, which publishes filler video to it for a few seconds); \
                         give --upload-kbps or test first"
                            .into(),
                    );
                    None
                }
                None => {
                    let test = self.speed_test(SpeedTestParams {
                        service: Some(service),
                        server: server.clone(),
                        seconds: None,
                    })?;
                    if let Some(reason) = &test.reason {
                        notes.push(reason.clone());
                    }
                    let kbps = test.upload_kbps;
                    speed = Some(test);
                    kbps
                }
            };
            Some(StreamChoice {
                service,
                server,
                upload_kbps,
            })
        } else {
            None
        };
        let (prepared, prepare_notes) = optimize::prepare(&live, params.goal, choice.as_ref());
        notes.extend(prepare_notes);
        let problems = prepared.validate();
        if !problems.is_empty() {
            let lines: Vec<String> = problems
                .iter()
                .map(|i| format!("{}: {}", i.path, i.message))
                .collect();
            return Err(failed(format!(
                "the optimized setup has problems, so nothing changed: {}",
                lines.join("; ")
            )));
        }
        let tuning = self.tune(TuneParams {
            setup: Some(prepared.clone()),
            folder: params.folder.clone(),
        })?;
        let to_json =
            |setup: &Setup| serde_json::to_value(setup).map_err(|e| failed(e.to_string()));
        let mut result = prepared;
        if let Some(change) = &tuning.change {
            let merged = merge_patch(&to_json(&result)?, change);
            result = serde_json::from_value(merged)
                .map_err(|err| failed(format!("tune's change didn't fit the setup: {err}")))?;
        }
        if params.goal.streams() {
            notes.push(
                "the stream's H.264 encoder isn't part of the benchmark; watch for dropped \
                 frames in the status bar on the first stream"
                    .into(),
            );
        }
        let differences = diff(&to_json(&live)?, &to_json(&result)?);
        let summary = optimize::summary(&result, params.goal);
        let applied = if params.apply && !differences.is_empty() {
            Some(self.apply(ApplyParams {
                revision,
                change: Change::Setup(Box::new(result.clone())),
                live: false,
            })?)
        } else {
            None
        };
        Ok(Optimized {
            goal: params.goal,
            summary,
            setup: result,
            differences,
            tuning,
            speed,
            notes,
            applied,
        })
    }
}
