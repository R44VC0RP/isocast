//! Engine monitoring (hearing a source or the program) through a PipeWire
//! playback stream. Like capture, every PipeWire object stays on one owned
//! worker running its main loop; the stream has no RT_PROCESS, so the
//! engine's render callback runs on that worker. Each buffer is filled
//! without allocating or locking: a scratch buffer made at start, zeroed,
//! handed to the engine's lock-free renderer, then copied out.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use isocast_engine::AudioRender;
use isocast_protocol::devices::AudioOutput;
use pipewire as pw;
use pw::properties::properties;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::Pod;
use pw::{spa, stream::StreamState};

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const FRAME_BYTES: usize = CHANNELS as usize * 4;
/// The most frames one buffer is filled with (PipeWire asks for its
/// quantum, 1024 by default; 8192 is its largest).
const MAX_FRAMES: usize = 8192;
/// The quantum asked for: about 21 ms, steady in a VM without a GPU.
const LATENCY: &str = "1024/48000";
const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

/// A playing monitor output; dropping it stops and joins its worker.
pub struct Playback {
    cancel: Arc<AtomicBool>,
    /// Cleared when the worker ends on its own (the output went away, or
    /// none came within `TIMEOUT`).
    alive: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Playback {
    /// Whether it still plays: false once its stream failed, so the
    /// monitor can open the device again.
    #[allow(
        dead_code,
        reason = "the engine's Playback is opaque; for a monitor that reopens"
    )]
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// Which output `device` names ("default", or an output's id, name or part
/// of its name, as `devices` lists them): the node to target (none for the
/// default, which PipeWire follows when it changes) and its name.
fn resolve(device: &str, outputs: &[AudioOutput]) -> Result<(Option<String>, String)> {
    if matches!(device, "default" | "") {
        let name = outputs.iter().find(|output| output.default).map_or_else(
            || "Default output".to_string(),
            |output| output.name.clone(),
        );
        return Ok((None, name));
    }
    let lower = device.to_lowercase();
    let found = outputs
        .iter()
        .find(|output| output.id == device || output.name == device)
        .or_else(|| {
            outputs
                .iter()
                .find(|output| output.name.to_lowercase().contains(&lower))
        });
    match found {
        Some(output) => Ok((Some(output.id.clone()), output.name.clone())),
        None => {
            let names: Vec<&str> = outputs.iter().map(|output| output.name.as_str()).collect();
            bail!(
                "audio.monitoring_device: no output {device:?}; valid values are \"default\" or one of {names:?}"
            )
        }
    }
}

/// Start playing what `render` fills on `device`. The caller is a source's
/// audio thread (the monitor opens with the first audio heard), whose
/// lateness the recording's mix drops: this returns once the stream is
/// made and connecting, without waiting for an output. "default" needs no
/// device list; a named device is looked up first. An output that doesn't
/// come within `TIMEOUT`, or goes later, ends playback with a warning.
pub fn play(device: &str, render: AudioRender) -> Result<(Playback, String)> {
    ensure!(
        !device.contains('\0'),
        "audio.monitoring_device contains NUL"
    );
    let (target, name) = if matches!(device, "default" | "") {
        (None, "Default output".to_string())
    } else {
        let (_, outputs, _) = crate::pipewire_audio::devices()?;
        resolve(device, &outputs)?
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));
    let (worker_cancel, worker_alive) = (cancel.clone(), alive.clone());
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("pipewire-playback".into())
        .spawn(move || {
            let mut started = false;
            let result =
                playback_worker(target.as_deref(), render, &worker_cancel, &tx, &mut started);
            worker_alive.store(false, Ordering::Release);
            if let Err(error) = result {
                if started {
                    if !worker_cancel.load(Ordering::Acquire) {
                        isocast_diagnostics::diagnostic!("warn"; "monitoring stopped: {error:#}");
                    }
                } else {
                    let _ = tx.send(Err(error));
                }
            }
        })
        .context("starting PipeWire playback worker")?;
    let playback = Playback {
        cancel,
        alive,
        worker: Some(worker),
    };
    rx.recv_timeout(TIMEOUT * 2)
        .context("PipeWire playback startup timed out or worker stopped")??;
    Ok((playback, name))
}

#[derive(Default)]
struct Status {
    connected: bool,
    error: Option<String>,
}

struct PlayData {
    render: AudioRender,
    status: Rc<RefCell<Status>>,
    /// Interleaved stereo, `MAX_FRAMES` long, made once.
    scratch: Vec<f32>,
}

fn playback_worker(
    target: Option<&str>,
    render: AudioRender,
    cancel: &AtomicBool,
    ready: &mpsc::SyncSender<Result<()>>,
    started: &mut bool,
) -> Result<()> {
    pw::init();
    let mut props = properties! {
        "media.type" => "Audio",
        "media.category" => "Playback",
        "media.role" => "Production",
        "node.name" => "isocast.monitor",
        "node.description" => "Isocast monitor",
        "node.latency" => LATENCY,
    };
    if let Some(target) = target {
        props.insert("target.object", target);
    }
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("connecting monitoring playback to PipeWire")?;
    let status = Rc::new(RefCell::new(Status::default()));
    let errors = status.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| {
            errors.borrow_mut().error = Some(message.to_owned());
        })
        .register();
    let stream = pw::stream::StreamBox::new(&core, "Isocast monitor", props)?;
    let data = PlayData {
        render,
        status: status.clone(),
        scratch: vec![0.0; MAX_FRAMES * CHANNELS as usize],
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, state| match state {
            StreamState::Error(error) => data.status.borrow_mut().error = Some(error),
            StreamState::Unconnected => {
                data.status.borrow_mut().error = Some("playback stream disconnected".into())
            }
            // Paused is reached without an output too (no session manager
            // to link one): only Streaming means one plays it.
            StreamState::Streaming => data.status.borrow_mut().connected = true,
            StreamState::Connecting | StreamState::Paused => {}
        })
        .process(fill)
        .register()?;
    let mut format = AudioInfoRaw::new();
    format.set_format(AudioFormat::F32LE);
    format.set_rate(RATE);
    format.set_channels(CHANNELS);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    format.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| anyhow!("serializing PipeWire audio format: {error:?}"))?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&bytes).context("invalid serialized PipeWire audio format")?;
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [pod],
    )?;
    // Made and connecting: the caller goes on; an output arrives (or not)
    // on this thread.
    ready
        .send(Ok(()))
        .context("PipeWire playback caller disconnected")?;
    *started = true;
    let deadline = Instant::now() + TIMEOUT;
    let mut connected = false;
    while !cancel.load(Ordering::Acquire) {
        let result = mainloop.loop_().iterate(pw::loop_::Timeout::Finite(POLL));
        ensure!(
            result >= 0 || result == -libc::EINTR,
            "PipeWire loop failed: {result}"
        );
        let mut state = status.borrow_mut();
        if let Some(error) = state.error.take() {
            bail!("{error}");
        }
        if !connected {
            connected = state.connected;
            ensure!(
                connected || Instant::now() < deadline,
                "PipeWire found no output to play monitoring through within 5 seconds; check the session manager (wireplumber) and the device"
            );
        }
    }
    Ok(())
}

/// Fill one buffer: as many frames as PipeWire asks for (its quantum),
/// bounded by the buffer and the scratch.
fn fill(stream: &pw::stream::Stream, data: &mut PlayData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = usize::try_from(buffer.requested()).unwrap_or(usize::MAX);
    let Some(first) = buffer.datas_mut().first_mut() else {
        return;
    };
    // An unmapped buffer goes back empty, never with an old chunk's size.
    let frames = 'fill: {
        let Some(bytes) = first.data() else {
            break 'fill 0;
        };
        let room = (bytes.len() / FRAME_BYTES).min(MAX_FRAMES);
        let frames = if requested == 0 {
            room
        } else {
            requested.min(room)
        };
        render_into(
            &mut data.render,
            &mut data.scratch,
            &mut bytes[..frames * FRAME_BYTES],
        );
        frames
    };
    let chunk = first.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = FRAME_BYTES as i32;
    *chunk.size_mut() = (frames * FRAME_BYTES) as u32;
}

/// Zero `scratch`'s first `out.len() / 4` samples, let `render` add into
/// them (the engine's renderer adds each source it mixes), and write them
/// to `out` as little-endian floats, non-finite ones as silence.
fn render_into(render: &mut AudioRender, scratch: &mut [f32], out: &mut [u8]) {
    let samples = &mut scratch[..out.len() / 4];
    samples.fill(0.0);
    render(samples);
    for (bytes, sample) in out.as_chunks_mut::<4>().0.iter_mut().zip(samples.iter()) {
        let sample = if sample.is_finite() { *sample } else { 0.0 };
        *bytes = sample.to_le_bytes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(id: &str, name: &str, default: bool) -> AudioOutput {
        AudioOutput {
            id: id.into(),
            name: name.into(),
            default,
            sample_rate: 48_000.0,
            channels: 2,
        }
    }

    #[test]
    fn resolves_default_ids_names_and_parts_of_names() {
        let outputs = [
            output(
                "alsa_output.pci.analog",
                "Built-in Audio Analog Stereo",
                true,
            ),
            output("bluez_output.headphones", "WH-1000XM4", false),
        ];
        assert_eq!(
            resolve("default", &outputs).unwrap(),
            (None, "Built-in Audio Analog Stereo".into())
        );
        assert_eq!(
            resolve("default", &[]).unwrap(),
            (None, "Default output".into())
        );
        assert_eq!(
            resolve("bluez_output.headphones", &outputs)
                .unwrap()
                .0
                .as_deref(),
            Some("bluez_output.headphones")
        );
        assert_eq!(
            resolve("wh-1000", &outputs).unwrap(),
            (Some("bluez_output.headphones".into()), "WH-1000XM4".into())
        );
        let error = resolve("HDMI", &outputs).unwrap_err().to_string();
        assert!(
            error.contains("audio.monitoring_device") && error.contains("WH-1000XM4"),
            "{error}"
        );
    }

    #[test]
    fn renders_onto_silence_and_drops_non_finite_samples() {
        // The engine's renderer adds into the buffer, so it must start silent.
        let mut render: AudioRender = Box::new(|out: &mut [f32]| {
            for (n, sample) in out.iter_mut().enumerate() {
                *sample += if n == 3 { f32::NAN } else { 0.25 };
            }
        });
        let mut scratch = vec![9.0; 8];
        let mut out = vec![0u8; 4 * 4];
        render_into(&mut render, &mut scratch, &mut out);
        let samples: Vec<f32> = out
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        assert_eq!(samples, [0.25, 0.25, 0.25, 0.0]);
    }
}
