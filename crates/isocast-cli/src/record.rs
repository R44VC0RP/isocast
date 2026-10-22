//! Foreground recording through the same API as `start`, but with a private
//! daemon. Neither the running app's setup nor its active outputs are touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_client::{DaemonError, IsolatedDaemon};
use isocast_protocol::Setup;
use isocast_protocol::api::{
    EngineState, Event, EventKind, Method, Report, RpcError, StartParams, Started, Status,
    WatchParams,
};

pub(super) fn run(
    setup: Setup,
    folder: PathBuf,
    seconds: Option<f64>,
    json: bool,
) -> Result<(Report, Option<anyhow::Error>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
        .context("could not install the Ctrl-C handler")?;
    let owner = IsolatedDaemon::launch()?;
    let result = record(&owner, setup, folder, seconds, json, &stop);
    // All early errors close the lifetime pipe too. Never kill a child still
    // finalizing files; close reports a slow shutdown and preserves its logs.
    let closed = if result.is_ok() {
        owner.close()
    } else {
        owner.close_preserving()
    };
    match (result, closed) {
        // Never throw away a completed take's report because teardown failed.
        // The caller prints it, then reports the independent cleanup failure.
        (Ok(report), closed) => Ok((report, closed.err())),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(close)) => {
            Err(error.context(format!("isolated daemon shutdown: {close:#}")))
        }
    }
}

fn record(
    owner: &IsolatedDaemon,
    setup: Setup,
    folder: PathBuf,
    seconds: Option<f64>,
    json: bool,
    stop: &AtomicBool,
) -> Result<Report> {
    let mut control = owner.connect()?;
    // Subscribe before Start: even a very short take must deliver its report.
    let (_, events) = owner.connect()?.watch_some(Some(WatchParams {
        kinds: Some(vec![
            EventKind::Started,
            EventKind::Warning,
            EventKind::Stopped,
        ]),
        except: Vec::new(),
    }))?;
    let (events_tx, events_rx) = mpsc::sync_channel(16);
    thread::Builder::new()
        .name("record events".into())
        .spawn(move || {
            for event in events {
                let finished = matches!(&event, Ok(Event::Stopped { .. }) | Err(_));
                if events_tx.send(event).is_err() || finished {
                    return;
                }
            }
            let _ = events_tx.send(Err(anyhow::anyhow!(
                "isolated daemon closed the recording event stream"
            )));
        })?;
    if stop.load(Ordering::SeqCst) {
        bail!("recording stopped before it started");
    }
    let mut start = owner.connect()?;
    let (started_tx, started_rx) = mpsc::channel();
    // Start may wait on device initialization. Keep Ctrl-C responsive while
    // that request runs; it uses another connection to stop only this daemon.
    thread::Builder::new()
        .name("record start".into())
        .spawn(move || {
            let result: Result<Started> = start.call(Method::Start(Box::new(StartParams {
                setup: Some(setup),
                folder: Some(folder),
                seconds,
                new_project: true,
                into: None,
            })));
            let _ = started_tx.send(result);
        })?;
    let mut starting = true;
    let mut polling = false;
    let mut stopping = false;
    loop {
        if starting {
            match started_rx.try_recv() {
                Ok(result) => {
                    starting = false;
                    if let Err(error) = result {
                        let status: Status = control.call(Method::Status)?;
                        if status.state == EngineState::Idle {
                            if let Some(report) =
                                status.last.filter(|_| status.last_error.is_none())
                            {
                                return Ok(report);
                            }
                            return Err(error);
                        }
                        // Start also has a bounded API wait. Its timeout is
                        // not a failed recording if the private session lives.
                        isocast_diagnostics::diagnostic!("warn"; "recording is still starting ({error:#}); following status");
                        polling = true;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    bail!("recording start worker exited without a result")
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if polling || stop.load(Ordering::SeqCst) {
            let status: Status = control.call(Method::Status)?;
            if status.state == EngineState::Idle {
                if let Some(error) = status.last_error {
                    bail!("{error}");
                }
                if let Some(report) = status.last {
                    return Ok(report);
                }
                if !starting {
                    bail!("recording ended without a report");
                }
                // Ctrl-C may beat Start's session creation. Keep waiting and
                // cancel once it exists, rather than misreporting a finished
                // short take as one which never started.
            } else if stop.load(Ordering::SeqCst) && !stopping {
                match control.call::<Report>(Method::Stop(None)) {
                    Ok(report) => return Ok(report),
                    Err(error)
                        if error
                            .downcast_ref::<DaemonError>()
                            .is_some_and(|error| error.code() == RpcError::WRONG_STATE) =>
                    {
                        // Auto-stop won the race. Status retains its report.
                        continue;
                    }
                    Err(error) => {
                        // Stop's bounded API wait can expire while durable
                        // finalization is still running. Foreground record
                        // must keep waiting for the report, as it did in-process.
                        let status: Status = control.call(Method::Status)?;
                        if status.state == EngineState::Idle {
                            if let Some(report) =
                                status.last.filter(|_| status.last_error.is_none())
                            {
                                return Ok(report);
                            }
                            return Err(error);
                        }
                        isocast_diagnostics::diagnostic!("warn"; "recording is still finishing ({error:#}); waiting for its report");
                        stopping = true;
                        polling = true;
                    }
                }
            }
            if polling {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        match events_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(Event::Started { files, .. })) if !json => {
                match seconds {
                    Some(seconds) => isocast_diagnostics::diagnostic!("Recording for {seconds} s:"),
                    None => isocast_diagnostics::diagnostic!("Recording, press Ctrl-C to stop:"),
                }
                for path in files {
                    isocast_diagnostics::diagnostic!("  {}", path.display());
                }
            }
            Ok(Ok(Event::Warning { message })) if !json => {
                isocast_diagnostics::diagnostic!("warn"; "warning: {message}");
            }
            Ok(Ok(Event::Stopped { report, error, .. })) => {
                if let Some(error) = error {
                    bail!("{error}");
                }
                return report
                    .map(|report| *report)
                    .context("recording stopped without a report");
            }
            Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => {
                isocast_diagnostics::diagnostic!("warn"; "recording events lost ({error:#}); following status instead");
                polling = true;
            }
            Err(RecvTimeoutError::Disconnected) => {
                isocast_diagnostics::diagnostic!("warn"; "recording events disconnected; following status instead");
                polling = true;
            }
        }
    }
}
