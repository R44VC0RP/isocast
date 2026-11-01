//! Hardware qualification tap, never run automatically: capture one selected
//! output's actual WASAPI mix as stereo 48 kHz f32le for offline pulse checks.
//! Usage: monitor-loopback DEVICE SECONDS OUTPUT.f32

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::io::Write;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use anyhow::{Context, ensure};
    use isocast_engine::{AudioSink, SAMPLE_RATE};
    use isocast_platform_windows::capture_loopback;

    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: monitor-loopback DEVICE SECONDS OUTPUT.f32"
    );
    let seconds: u32 = args[2].parse().context("SECONDS must be 1..=600")?;
    ensure!((1..=600).contains(&seconds), "SECONDS must be 1..=600");
    // Reserve before capture. A bounded scratch fixture, not a production
    // writer: no allocation or disk writes in the native packet callback.
    let limit = (seconds as usize + 2) * SAMPLE_RATE as usize * 2;
    let samples = Arc::new(Mutex::new(Vec::with_capacity(limit)));
    let output = samples.clone();
    let (errors, stopped) = mpsc::channel();
    let overflow = errors.clone();
    let (capture, channels) = capture_loopback(
        &args[1],
        AudioSink {
            report_gap: Box::new(|_, _| {}),
            samples: Box::new(move |packet, _| {
                let mut samples = output.lock().unwrap();
                if samples.len() + packet.len() <= limit {
                    samples.extend_from_slice(packet);
                } else {
                    let _ = overflow.send("loopback fixture exceeded its bounded capacity".into());
                }
            }),
            stopped: Box::new(move |error| {
                let _ = errors.send(error);
            }),
        },
        || {
            if let Err(error) = isocast_platform_windows::audio_thread() {
                eprintln!("loopback scheduling: {error:#}");
            }
        },
    )?;
    ensure!(channels == 2, "loopback must deliver stereo");
    eprintln!("loopback ready: {:?}", capture.device());
    let error = stopped
        .recv_timeout(Duration::from_secs(u64::from(seconds)))
        .ok();
    let stats = capture.stats();
    drop(capture);
    eprintln!("loopback stats: {stats:?}");
    let samples = samples.lock().unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[3])?;
    let mut file = std::io::BufWriter::new(file);
    for sample in samples.iter() {
        file.write_all(&sample.to_le_bytes())?;
    }
    file.flush()?;
    ensure!(
        error.is_none(),
        "loopback stopped: {}",
        error.unwrap_or_default()
    );
    ensure!(
        stats.gap_frames == 0 && stats.timestamp_errors == 0,
        "loopback did not capture continuously: {stats:?}"
    );
    ensure!(!samples.is_empty(), "loopback received no audio");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("monitor-loopback requires Windows and an output device");
    std::process::exit(1);
}
