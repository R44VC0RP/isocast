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
                // The home folder as this OS has it (USERPROFILE or the
                // profile known folder on Windows, where HOME is unset).
                let home = Some(isocast_protocol::api::home_folder())
                    .filter(|home| !home.as_os_str().is_empty());
                match (std::env::var_os(obs::FOLDER_ENV), home) {
                    (Some(folder), _) if !folder.is_empty() => PathBuf::from(folder),
                    (_, Some(home)) => obs::obs_folder(&home),
                    _ => {
                        return Err(failed(
                            "folder: isocastd can't find this user's home folder, so give \
                             OBS's settings folder",
                        ));
                    }
                }
            }
        };
        Ok(obs::discover(&folder))
    }

    pub(crate) fn speed_test(&self, params: SpeedTestParams) -> Result<SpeedTest, RpcError> {
        // Held from choosing the destination until its key is in hand,
        // migration included: destination changes (and their key clearing)
        // and key provisioning take this lock too, so the key is the one
        // for the destination checked, never one set after a network
        // caller's server became current (decision 26). Released before
        // the upload.
        let state = self.state.lock();
        if state.streaming() {
            return Err(wrong_state(
                "not while streaming: the test would compete with the stream for the upload",
            ));
        }
        let live = stream_settings(&state);
        // The bitrate is for the canvas's rate, as optimize picks it.
        let fps = state.setup.as_ref().map_or(60, |s| s.canvas.fps);
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
        // A network caller's test goes only to the configured stream
        // destination (decision 25): it publishes with the stored key, so a
        // server or service chosen from another computer could receive it.
        // Checked here, so `optimize` and anything else that tests the
        // upload are held to it too.
        if !crate::callers::current().is_local() {
            let configured = live.as_ref().filter(|l| l.service == service);
            let server_ok = params
                .server
                .as_ref()
                .is_none_or(|given| configured.is_some_and(|l| l.server.as_ref() == Some(given)));
            if configured.is_none() || !server_ok {
                return Err(RpcError {
                    code: RpcError::NOT_PERMITTED,
                    message: format!(
                        "an upload test from another computer goes only to the live setup's \
                         configured stream destination (it publishes with the stored stream \
                         key); leave out service and server, or test another server on {}",
                        crate::callers::machine_name()
                    ),
                });
            }
        }
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
            _ => stream_key(&state, &account).map_err(crate::failed)?,
        };
        drop(state);
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
        let state = self.state.read();
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
        // From another computer it can't name a server (decision 25): the
        // stream would then publish the stored key there.
        if !crate::callers::current().is_local()
            && let Some(server) = &params.server
            && live.stream.as_ref().and_then(|s| s.server.as_ref()) != Some(server)
        {
            return Err(RpcError {
                code: RpcError::NOT_PERMITTED,
                message: format!(
                    "server: optimize from another computer uses the live setup's configured \
                     stream destination; leave out server, or choose another on {}",
                    crate::callers::machine_name()
                ),
            });
        }
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
        // A benchmark that couldn't run measured nothing: say why, rather
        // than report "nothing to change" as if the setup had been judged.
        if tuning.candidates.iter().all(|c| c.capacity.is_none()) && tuning.variants.is_empty() {
            let reason = tuning
                .candidates
                .first()
                .and_then(|c| c.reason.as_deref())
                .unwrap_or("unknown error");
            return Err(failed(format!(
                "the benchmark could not run, so nothing was optimized: {reason}"
            )));
        }
        // A source that couldn't be opened is a warning of its own, first:
        // the rest was still optimized.
        for (at, skip) in tuning.skipped.iter().enumerate() {
            notes.insert(
                at,
                format!(
                    "Source {:?} couldn't be opened, so its ISO file was left out of the \
                     benchmark and the rest was optimized without it: {}",
                    skip.source, skip.reason
                ),
            );
        }
        let to_json =
            |setup: &Setup| serde_json::to_value(setup).map_err(|e| failed(e.to_string()));
        let mut result = prepared;
        if let Some(change) = &tuning.change {
            let merged = merge_patch(&to_json(&result)?, change);
            result = serde_json::from_value(merged)
                .map_err(|err| failed(format!("tune's change didn't fit the setup: {err}")))?;
        }
        // The setup benchmark doesn't include a live stream. A qualified
        // local machine profile also judges the combined workload, using
        // the exact same suggestions/admission model as `capacity`.
        // Respect the selected exact pass, including a different codec or
        // smaller variant that tune benchmarked successfully.
        let tuned_fits = tuning.candidates.iter().any(|candidate| {
            candidate.feasible && tuning.recommended.as_ref() == Some(&candidate.encoder)
        }) || tuning
            .variants
            .iter()
            .any(|variant| variant.feasible && tuning.change.as_ref() == Some(&variant.change));
        if let Ok(estimate) = self.estimate_capacity(isocast_protocol::api::CapacityParams {
            setup: Some(result.clone()),
            stream: Some(params.goal.streams()),
            add_iso: None,
        }) && (estimate.basis.starts_with("measured: local")
            || estimate.basis.starts_with("mixed: measured local")
            || !estimate.resources.is_empty())
        {
            notes.push(format!("Machine profile: {}", estimate.summary));
            if estimate.basis.starts_with("mixed:") {
                notes.push("The mixed estimate measures HEVC costs locally; H.264 recording and live costs remain per-chip table assumptions".into());
            }
            if (params.goal.streams() || !tuned_fits)
                && let Some(suggestion) = estimate.suggestions.iter().find(|s| {
                    s.capacity == isocast_protocol::api::Capacity::Fits && s.change.is_some()
                })
            {
                result = Setup::from_value(merge_patch(
                    &to_json(&result)?,
                    suggestion.change.as_ref().unwrap(),
                ))
                .map_err(|err| {
                    failed(format!(
                        "machine profile's change didn't fit the setup: {err}"
                    ))
                })?;
                notes.push(format!(
                    "Machine profile recommends: {}",
                    suggestion.description
                ));
            } else if !params.goal.streams() && tuned_fits && !estimate.suggestions.is_empty() {
                notes.push("Tune measured these files with headroom; keeping its result rather than applying the conservative machine-profile suggestion".into());
            }
        }
        // ISO frame thinning is the user's choice: Optimize keeps the live
        // setting, never counts it toward a fit, and only mentions it.
        result.governor = live.governor.clone();
        if result.recording.iso.enabled && !result.recording.iso.sources.is_empty() {
            notes.push(if result.governor.allow_iso_thinning {
                "ISO frame thinning is allowed in this setup by your choice, so automatic \
                 quality may skip ISO pictures under pressure (lost for good); Optimize left \
                 it as it is and didn't count it toward the fit"
                    .into()
            } else {
                format!(
                    "Automatic quality keeps ISO files at full frame rate. {}; Optimize never \
                     turns it on",
                    isocast_engine::estimate::ISO_THINNING_NOTE
                )
            });
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
