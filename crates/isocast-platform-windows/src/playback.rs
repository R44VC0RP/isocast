//! Shared, event-driven WASAPI monitoring. Endpoint discovery starts with
//! the platform, not with the first monitored audio packet. `play` only
//! reads that cache and starts a worker: no COM/driver call or startup wait
//! can delay the recording's source worker. The output worker owns all COM
//! objects, retries lost devices and follows the default without ending a source.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use isocast_engine::{AudioRender, SAMPLE_RATE};
use parking_lot::Mutex;
use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioRenderClient, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Threading::{WaitForMultipleObjects, WaitForSingleObject};

use crate::audio::{default_id, endpoint_identities, enumerator, selected_endpoint};
use crate::native::{Apartment, Event, audio_thread};

const BUFFER: i64 = 300_000; // 30 ms, in 100 ns units
const RETRY_MS: u32 = 500;

#[derive(Clone, Debug, PartialEq)]
struct Output {
    id: String,
    name: String,
    default: bool,
}

struct Discovery {
    devices: Mutex<std::result::Result<Vec<Output>, String>>,
    revision: AtomicU64,
}

/// Read-only endpoint identities, refreshed off the audio path. Constructed
/// at platform startup (including the initial discovery), before any source
/// exists. Polling identities does not activate audio clients or change settings.
pub(crate) struct Outputs {
    state: Arc<Discovery>,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Default for Outputs {
    fn default() -> Self {
        let state = Arc::new(Discovery {
            devices: Mutex::new(Err(
                "audio.monitoring_device: output discovery is starting".into()
            )),
            revision: AtomicU64::new(0),
        });
        let (stop, stopped) = mpsc::channel();
        let (ready, initial) = mpsc::sync_channel(1);
        let worker_state = state.clone();
        let worker = thread::Builder::new()
            .name("monitor devices".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let _apartment = Apartment::new()?;
                    let mut last_error = String::new();
                    loop {
                        let result = (|| -> Result<Vec<Output>> {
                            // Recreate after an audio-service restart rather
                            // than retaining a dead service proxy forever.
                            let devices = enumerator()?;
                            let default = default_id(&devices, eRender)?;
                            Ok(endpoint_identities(&devices, eRender, false)?
                                .into_iter()
                                .map(|device| Output {
                                    default: default.as_deref() == Some(device.id.as_str()),
                                    id: device.id,
                                    name: device.name,
                                })
                                .collect())
                        })()
                        .map_err(|error| {
                            format!("audio.monitoring_device: output discovery failed: {error:#}")
                        });
                        match result {
                            Ok(devices) => {
                                last_error.clear();
                                let mut cache = worker_state.devices.lock();
                                if cache.as_ref().ok() != Some(&devices) {
                                    *cache = Ok(devices);
                                    worker_state.revision.fetch_add(1, Ordering::Release);
                                }
                            }
                            Err(error) => {
                                if last_error != error {
                                    isocast_diagnostics::diagnostic!("warn"; "{error}; retaining last known monitoring outputs");
                                    last_error = error.clone();
                                }
                                let mut cache = worker_state.devices.lock();
                                if cache.is_err() { *cache = Err(error); }
                            }
                        }
                        let _ = ready.try_send(());
                        if !matches!(
                            stopped.recv_timeout(Duration::from_secs(1)),
                            Err(mpsc::RecvTimeoutError::Timeout)
                        ) {
                            break;
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    *worker_state.devices.lock() = Err(format!(
                        "audio.monitoring_device: output discovery failed: {error:#}"
                    ));
                    let _ = ready.try_send(());
                }
            });
        let worker = match worker {
            Ok(worker) => {
                // Only platform construction waits. A slow/broken audio service
                // must not prevent the engine from starting to record.
                let _ = initial.recv_timeout(Duration::from_secs(2));
                Some(worker)
            }
            Err(error) => {
                *state.devices.lock() = Err(format!(
                    "audio.monitoring_device: cannot start discovery: {error}"
                ));
                None
            }
        };
        Self {
            state,
            stop,
            worker,
        }
    }
}

impl Drop for Outputs {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn select(query: &str, devices: &[Output]) -> Result<Output> {
    if devices.is_empty() {
        bail!(
            "audio.monitoring_device: no output device; connect or enable a Windows sound output"
        );
    }
    if query.is_empty() || query.eq_ignore_ascii_case("default") {
        return devices
            .iter()
            .find(|device| device.default)
            .cloned()
            .context("audio.monitoring_device: no output device selected as the Windows default");
    }
    if let Some(device) = devices.iter().find(|device| device.id == query) {
        return Ok(device.clone());
    }
    let lower = query.to_lowercase();
    let exact = devices
        .iter()
        .any(|device| device.name.to_lowercase() == lower);
    let mut matches = devices.iter().filter(|device| {
        let name = device.name.to_lowercase();
        if exact {
            name == lower
        } else {
            name.contains(&lower)
        }
    });
    if let Some(device) = matches.next()
        && matches.next().is_none()
    {
        return Ok(device.clone());
    }
    bail!(
        "audio.monitoring_device: no unique output {query:?}; valid values are default or an endpoint id from devices: {:?}",
        devices
            .iter()
            .map(|device| (&device.id, &device.name))
            .collect::<Vec<_>>()
    );
}

/// A named endpoint keeps its identity across removal. Default is only a
/// temporary fallback; plugging the preferred endpoint back in restores it.
fn target<'a>(preferred: Option<&str>, devices: &'a [Output]) -> Option<&'a Output> {
    preferred
        .and_then(|id| devices.iter().find(|device| device.id == id))
        .or_else(|| devices.iter().find(|device| device.default))
}

pub struct Playback {
    stop: Arc<AtomicBool>,
    wake: Arc<Event>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wake.signal();
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

pub(crate) fn play(
    outputs: &Arc<Outputs>,
    query: &str,
    render: AudioRender,
) -> Result<(Playback, String)> {
    ensure!(
        !query.contains('\0'),
        "audio.monitoring_device contains NUL"
    );
    let selected = {
        let cache = outputs.state.devices.lock();
        select(query, cache.as_ref().map_err(|error| anyhow!("{error}"))?)?
    };
    let name = selected.name.clone();
    let follow_default = query.is_empty() || query.eq_ignore_ascii_case("default");
    let stop = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Event::new()?);
    let (worker_stop, worker_wake, discovery) = (stop.clone(), wake.clone(), outputs.clone());
    let worker = thread::Builder::new()
        .name("WASAPI monitor".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    &discovery.state,
                    selected,
                    follow_default,
                    render,
                    &worker_stop,
                    &worker_wake,
                )
            }));
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!("Windows monitoring stopped: {error:#}"),
                Err(_) => "Windows monitoring worker panicked".into(),
            };
            isocast_diagnostics::diagnostic!("warn"; "{error}");
        })
        .context("starting WASAPI monitor worker")?;
    Ok((
        Playback {
            stop,
            wake,
            worker: Some(worker),
        },
        name,
    ))
}

fn run(
    discovery: &Discovery,
    mut selected: Output,
    follow_default: bool,
    mut render: AudioRender,
    stop: &AtomicBool,
    wake: &Event,
) -> Result<()> {
    let _apartment = Apartment::new()?;
    if let Err(error) = audio_thread() {
        isocast_diagnostics::diagnostic!("warn"; "Windows monitoring MMCSS Audio unavailable: {error:#}");
    }
    let preferred = (!follow_default).then(|| selected.id.clone());
    let mut previous_error = String::new();
    let mut retry_ms = RETRY_MS;
    while !stop.load(Ordering::Acquire) {
        let result = (|| -> Result<()> {
            let next = {
                let cache = discovery.devices.lock();
                let cache = cache.as_ref().map_err(|error| anyhow!("{error}"))?;
                target(preferred.as_deref(), cache)
                    .context("audio.monitoring_device: no output device; waiting for a Windows sound output")?.clone()
            };
            if next.id != selected.id {
                isocast_diagnostics::diagnostic!("warn"; "Windows monitoring switching from {:?} to {:?}; preferred endpoint is retained", selected.name, next.name);
            }
            selected = next;
            let devices = enumerator()?;
            let endpoint = selected_endpoint(&devices, eRender, &selected.id)?;
            let mut session =
                Session::new(endpoint.client, endpoint.device.channels, endpoint.mask)
                    .with_context(|| format!("audio.monitoring_device {:?}", selected.name))?;
            isocast_diagnostics::diagnostic!("info"; "Windows monitoring playing on {:?} ({}), shared buffer {:.2} ms", selected.name, selected.id, f64::from(session.frames) * 1000.0 / f64::from(SAMPLE_RATE));
            let result = session.run(
                &mut render,
                stop,
                wake,
                discovery,
                &selected.id,
                preferred.as_deref(),
            );
            if session.played >= u64::from(SAMPLE_RATE) {
                retry_ms = RETRY_MS;
                previous_error.clear();
            }
            result
        })();
        if stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = result {
            let message = format!("{error:#}");
            if previous_error != message {
                isocast_diagnostics::diagnostic!("warn"; "Windows monitoring interrupted: {message}; retrying without stopping the source");
                previous_error = message;
            }
            let revision = discovery.revision.load(Ordering::Acquire);
            let until = Instant::now() + Duration::from_millis(u64::from(retry_ms));
            while !stop.load(Ordering::Acquire)
                && discovery.revision.load(Ordering::Acquire) == revision
                && Instant::now() < until
            {
                unsafe { WaitForSingleObject(wake.handle(), RETRY_MS) };
            }
            retry_ms = (retry_ms * 2).min(5000);
        } else {
            retry_ms = RETRY_MS;
        }
    }
    Ok(())
}

/// Stereo goes to front left/right only, never to LFE or surround. Mono
/// averages both sides. Missing/inconsistent masks use positional channel order.
struct Mapping {
    channels: usize,
    left: usize,
    right: usize,
}

impl Mapping {
    fn new(channels: u32, mask: u32) -> Result<Self> {
        ensure!(
            (1..=32).contains(&channels),
            "monitor mix channels {channels}: expected 1..=32"
        );
        if channels == 1 {
            return Ok(Self {
                channels: 1,
                left: 0,
                right: 0,
            });
        }
        ensure!(
            mask.count_ones() != channels || mask & 3 == 3,
            "monitor mix channel mask {mask:#x} has no front left/right pair"
        );
        Ok(Self {
            channels: channels as usize,
            left: 0,
            right: 1,
        })
    }

    fn convert(&self, stereo: &[f32], output: &mut [f32]) {
        output.fill(0.0);
        let clean = |sample: f32| {
            if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        };
        for (input, frame) in stereo
            .as_chunks::<2>()
            .0
            .iter()
            .zip(output.chunks_exact_mut(self.channels))
        {
            let (left, right) = (clean(input[0]), clean(input[1]));
            if self.channels == 1 {
                frame[0] = (left + right) * 0.5;
            } else {
                frame[self.left] = left;
                frame[self.right] = right;
            }
        }
    }
}

struct Session {
    client: IAudioClient,
    renderer: IAudioRenderClient,
    data: Event,
    frames: u32,
    mapping: Mapping,
    stereo: Vec<f32>,
    played: u64,
}

impl Session {
    fn new(client: IAudioClient, channels: u32, mask: u32) -> Result<Self> {
        let mapping = Mapping::new(channels, mask)?;
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: channels as u16,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * channels * 4,
                nBlockAlign: (channels * 4) as u16,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            // A malformed mask cannot describe this layout. Use direct-out
            // order, matching Mapping's positional left/right fallback.
            dwChannelMask: if mask.count_ones() == channels {
                mask
            } else {
                0
            },
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        // WASAPI converts float/48 kHz to the mix format/rate. We preserve
        // its layout rather than relying on an implicit stereo upmix.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER,
                0,
                &format.Format,
                None,
            )
        }
        .context("initialize monitor mix")?;
        let data = Event::new()?;
        unsafe { client.SetEventHandle(data.handle()) }?;
        let frames = unsafe { client.GetBufferSize() }?;
        ensure!(frames > 0, "monitor output returned an empty buffer");
        ensure!(
            frames <= SAMPLE_RATE / 10,
            "monitor buffer {frames} frames ({:.2} ms) exceeds 100 ms limit; choose another audio.monitoring_device",
            f64::from(frames) * 1000.0 / f64::from(SAMPLE_RATE)
        );
        let renderer = unsafe { client.GetService() }?;
        let session = Self {
            client,
            renderer,
            data,
            frames,
            mapping,
            stereo: vec![0.0; frames as usize * 2],
            played: 0,
        };
        // Prime one device period with silence before starting the event clock.
        // Do not consume the monitor's source buffers during device startup.
        let mut period = 0;
        unsafe { session.client.GetDevicePeriod(Some(&mut period), None) }?;
        ensure!(
            period > 0,
            "monitor output returned an invalid device period"
        );
        let prime = (period as u64)
            .saturating_mul(u64::from(SAMPLE_RATE))
            .div_ceil(10_000_000)
            .clamp(1, u64::from(frames)) as u32;
        unsafe {
            session.renderer.GetBuffer(prime)?;
            session
                .renderer
                .ReleaseBuffer(prime, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            session.client.Start()?;
        }
        Ok(session)
    }

    fn run(
        &mut self,
        render: &mut AudioRender,
        stop: &AtomicBool,
        wake: &Event,
        discovery: &Discovery,
        selected: &str,
        preferred: Option<&str>,
    ) -> Result<()> {
        let mut last_data = Instant::now();
        let mut revision = u64::MAX;
        while !stop.load(Ordering::Acquire) {
            // Control work is outside the fill. The unchanged fast path is
            // one atomic read, with no allocation, cache lock or COM discovery.
            if discovery.revision.load(Ordering::Acquire) != revision {
                // An unrelated input/output being plugged in must not break
                // this output's continuity. Reopen only if our target changed.
                let cache = discovery.devices.lock();
                revision = discovery.revision.load(Ordering::Acquire);
                let devices = cache.as_ref().map_err(|error| anyhow!("{error}"))?;
                // For a live preferred stream, native invalidation (or the
                // event watchdog), not a failed metadata read, proves removal.
                let unchanged = preferred == Some(selected)
                    || target(preferred, devices).is_some_and(|device| device.id == selected);
                if !unchanged {
                    return Ok(());
                }
            }
            let wait =
                unsafe { WaitForMultipleObjects(&[wake.handle(), self.data.handle()], false, 100) };
            if wait == WAIT_OBJECT_0 {
                return Ok(());
            }
            if wait == WAIT_FAILED {
                return Err(windows::core::Error::from_thread()).context("monitor event wait");
            }
            if wait.0 != WAIT_OBJECT_0.0 + 1 {
                ensure!(
                    last_data.elapsed() < Duration::from_secs(if self.played == 0 { 5 } else { 2 }),
                    "monitor output stopped requesting audio"
                );
                continue;
            }
            last_data = Instant::now();
            let padding = unsafe { self.client.GetCurrentPadding() }?;
            ensure!(padding <= self.frames, "monitor padding exceeds buffer");
            let frames = self.frames - padding;
            if frames == 0 {
                continue;
            }
            let stereo = &mut self.stereo[..frames as usize * 2];
            render(stereo);
            let buffer = unsafe { self.renderer.GetBuffer(frames) }?;
            // SAFETY: WASAPI leases frames * channels float samples until
            // ReleaseBuffer. Format and counts were validated at startup.
            let output = unsafe {
                std::slice::from_raw_parts_mut(
                    buffer.cast::<f32>(),
                    frames as usize * self.mapping.channels,
                )
            };
            self.mapping.convert(stereo, output);
            unsafe { self.renderer.ReleaseBuffer(frames, 0) }?;
            self.played += u64::from(frames);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stereo_mono_and_surround_without_lfe_or_surround_duplication() {
        for (channels, mask, expected) in [
            (1, 4, vec![0.25, -0.25]),
            (2, 3, vec![0.75, -0.25, -0.75, 0.25]),
            (2, 4, vec![0.75, -0.25, -0.75, 0.25]),
            (2, 0x3f, vec![0.75, -0.25, -0.75, 0.25]),
            (
                6,
                0x3f,
                vec![
                    0.75, -0.25, 0.0, 0.0, 0.0, 0.0, -0.75, 0.25, 0.0, 0.0, 0.0, 0.0,
                ],
            ),
        ] {
            let mapping = Mapping::new(channels, mask).unwrap();
            let mut out = vec![99.0; channels as usize * 2];
            mapping.convert(&[0.75, -0.25, -0.75, 0.25], &mut out);
            assert_eq!(out, expected);
        }
        for channels in 2..=32 {
            let mapping = Mapping::new(channels, 0).unwrap();
            let mut out = vec![99.0; channels as usize];
            mapping.convert(&[0.5, -0.25], &mut out);
            assert_eq!(&out[..2], &[0.5, -0.25]);
            assert!(out[2..].iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn conversion_sanitizes_nonfinite_and_clips_before_downmix() {
        let mut out = [99.0; 6];
        Mapping::new(2, 3)
            .unwrap()
            .convert(&[f32::NAN, f32::INFINITY, -2.0, 2.0, 0.1, -0.1], &mut out);
        assert_eq!(out, [0.0, 0.0, -1.0, 1.0, 0.1, -0.1]);
        let mut mono = [99.0];
        Mapping::new(1, 4)
            .unwrap()
            .convert(&[f32::MAX, f32::MAX], &mut mono);
        assert_eq!(mono, [1.0]);
        for (channels, mask) in [(0, 0), (33, 0), (2, 12)] {
            assert!(Mapping::new(channels, mask).is_err());
        }
    }

    #[test]
    fn target_follows_default_but_restores_a_named_output_after_removal() {
        let headphones = Output {
            id: "headphones".into(),
            name: "Headphones".into(),
            default: false,
        };
        let speakers = Output {
            id: "speakers".into(),
            name: "Speakers".into(),
            default: true,
        };
        let devices = [headphones.clone(), speakers.clone()];
        assert_eq!(target(None, &devices), Some(&speakers));
        assert_eq!(target(Some("headphones"), &devices), Some(&headphones));
        assert_eq!(
            target(Some("headphones"), std::slice::from_ref(&speakers)),
            Some(&speakers)
        );
        assert_eq!(target(Some("headphones"), &devices), Some(&headphones));
        let mut changed = devices.clone();
        changed[0].default = true;
        changed[1].default = false;
        assert_eq!(target(None, &changed), Some(&changed[0]));
        assert_eq!(target(Some("speakers"), &changed), Some(&changed[1]));
        assert!(target(Some("headphones"), &[]).is_none());
    }

    #[test]
    fn selection_reports_no_output_and_rejects_ambiguous_names() {
        assert!(
            select("default", &[])
                .unwrap_err()
                .to_string()
                .contains("no output device")
        );
        let first = Output {
            id: "a".into(),
            name: "Speakers".into(),
            default: true,
        };
        let second = Output {
            id: "b".into(),
            name: "Speakers".into(),
            default: false,
        };
        let devices = [first.clone(), second.clone()];
        assert_eq!(select("default", &devices).unwrap(), first);
        assert_eq!(select("b", &devices).unwrap(), second);
        assert!(select("Speakers", &devices).is_err());
        assert!(select("speak", &devices).is_err());
        assert_eq!(
            select("speak", std::slice::from_ref(&first)).unwrap(),
            first
        );
        assert_eq!(
            select("speakers", std::slice::from_ref(&first)).unwrap(),
            first
        );
    }
}
