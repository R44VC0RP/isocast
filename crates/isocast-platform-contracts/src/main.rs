//! Bounded by scripts/platform-contracts.py, one native case per process.
//! This binary never becomes a daemon, registers devices or changes defaults.

mod foundation;
mod media;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use isocast_engine::{Platform, ScreenTarget, VideoSink};
use isocast_protocol::devices::Permission;
use serde_json::{Value, json};

fn main() {
    let result = run();
    match result {
        Ok(value) => println!("{}", json!({"result": value})),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<Value> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 3,
        "use scripts/platform-contracts.py; worker arguments: backend case folder"
    );
    let backend = args[0].to_str().context("invalid backend")?;
    let case = args[1].to_str().context("invalid case")?;
    ensure!(
        matches!(
            case,
            "clock"
                | "pools"
                | "gpu"
                | "normal"
                | "pressure"
                | "commit"
                | "ownership"
                | "capture"
                | "recover"
        ),
        "unknown contract case {case}"
    );
    let folder = Path::new(&args[2]);
    ensure!(
        std::fs::read(folder.join(".contract-owner"))? == b"isocast-platform-contracts\n",
        "not a contract work folder"
    );
    match backend {
        "stub" => {
            if !matches!(case, "clock" | "pools") {
                return Ok(
                    json!({"unavailable": "StubPlatform has no native GPU, capture, encoder or writer; not native parity"}),
                );
            }
            dispatch(&isocast_platform_stub::StubPlatform, case, folder)
        }
        "native" => {
            #[cfg(target_os = "macos")]
            {
                dispatch(&isocast_platform_macos::MacPlatform, case, folder)
            }
            #[cfg(windows)]
            {
                dispatch(
                    &isocast_platform_windows::WindowsPlatform::default(),
                    case,
                    folder,
                )
            }
            #[cfg(not(any(target_os = "macos", windows)))]
            {
                Ok(
                    json!({"unavailable": "no native Platform implementation is selected on this OS; run stub/core only for the explicit CPU placeholder"}),
                )
            }
        }
        _ => bail!("backend: valid options are native, stub"),
    }
}

fn dispatch<P: Platform>(platform: &P, case: &str, folder: &Path) -> Result<Value> {
    match case {
        "clock" => foundation::clock(platform),
        "pools" => foundation::pools(platform),
        "gpu" => foundation::gpu(platform),
        "normal" | "pressure" | "commit" => media::record(platform, folder, case),
        "ownership" => media::ownership(platform, folder),
        "capture" => capture(platform),
        "recover" => {
            Ok(json!({"sessions": isocast_engine::record::recover_interrupted(platform, folder)}))
        }
        _ => bail!("unknown contract case {case}"),
    }
}

fn capture<P: Platform>(platform: &P) -> Result<Value> {
    let permission = platform.permissions()?.permissions.screen_recording;
    if permission != Permission::Granted {
        return Ok(
            json!({"unavailable": format!("screen recording permission is {}; contract never prompts", permission.label())}),
        );
    }
    let samples = Arc::new(Mutex::new(Vec::new()));
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let (ready, first) = mpsc::sync_channel(1);
    let count = Arc::new(AtomicUsize::new(0));
    let callbacks = count.clone();
    let received = samples.clone();
    let errors = stopped.clone();
    let clock = platform.clone();
    let sink = VideoSink {
        frame: Box::new(move |frame, time| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            let mut received = received.lock().unwrap();
            received.push((time, clock.now(), clock.frame_size(&frame)));
            // Bound retained observations even if a driver delivers far too fast.
            if received.len() > 1024 {
                received.remove(0);
            }
            let _ = ready.try_send(());
        }),
        stopped: Box::new(move |why| stopped.lock().unwrap().push(why)),
    };
    let (capture, format) =
        platform.capture_screen(ScreenTarget::Display("main"), 30, false, false, sink)?;
    first
        .recv_timeout(Duration::from_secs(5))
        .context("capture delivered no frame")?;
    std::thread::sleep(Duration::from_millis(100));
    drop(capture);
    let at_drop = count.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(150));
    let samples = samples.lock().unwrap();
    ensure!(
        count.load(Ordering::SeqCst) == at_drop,
        "capture called back after its handle was dropped"
    );
    ensure!(
        errors.lock().unwrap().is_empty(),
        "capture stopped unexpectedly"
    );
    ensure!(
        format.width > 0 && format.height > 0 && format.fps > 0,
        "capture reported an empty format"
    );
    // These are presentation timestamps, not callback-arrival timestamps.
    // ScreenCaptureKit delivers a frame before its scheduled display time
    // (measured 7.5–13.3 ms ahead on the test Mac). Allow one presentation
    // period, not an invented 10 ms latency target, while checking the epoch.
    let presentation_period = Duration::from_secs_f64(1.0 / f64::from(format.fps));
    let offsets: Vec<_> = samples
        .iter()
        .map(|(time, received, _)| time.as_nanos() - received.as_nanos())
        .collect();
    eprintln!("capture presentation-minus-arrival offsets (ns): {offsets:?}");
    for (index, (time, received, size)) in samples.iter().enumerate() {
        ensure!(
            *size == (format.width, format.height),
            "capture dimensions differ from its format"
        );
        ensure!(
            *time <= *received + presentation_period,
            "capture timestamp is ahead of the host clock: pts={} ns, arrived={} ns, delta={} ns",
            time.as_nanos(),
            received.as_nanos(),
            time.as_nanos() - received.as_nanos()
        );
        ensure!(
            received.saturating_since(*time) < Duration::from_secs(3),
            "capture timestamp uses another epoch or is stale"
        );
        if index > 0 {
            ensure!(
                *time >= samples[index - 1].0,
                "capture timestamps went backwards"
            );
        }
    }
    Ok(
        json!({"frames": samples.len(), "width": format.width, "height": format.height, "callbacks_after_drop": 0, "presentation_minus_arrival_ns": offsets}),
    )
}
