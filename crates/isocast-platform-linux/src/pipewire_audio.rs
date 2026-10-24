//! Native PipeWire capture. All PipeWire objects stay on their owning worker.
//!
//! Requires PipeWire headers/runtime >= 1.0.5 (per-buffer graph timestamps),
//! and pipewire-rs's `v0_3_77` feature (resampler-delay queries). Linux
//! has no audio playback yet, so excluding our own playback is vacuous; this
//! must change when playback/monitoring is implemented.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use isocast_engine::{AudioSink, Time};
use isocast_protocol as protocol;
use pipewire as pw;
use protocol::devices::{AudioOutput, Microphone};
use pw::properties::properties;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::Pod;
use pw::{spa, stream::StreamState};

const RATE: u32 = 48_000;
const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
const MAX_CHANNELS: u32 = 64;
// A malformed server buffer must not trigger an unbounded allocation.
const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub struct Capture {
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // A sink may remove its own source from its stopped callback.
            // Cancellation still ends that worker when the callback returns.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

pub fn microphone(
    device: &str,
    channels: Option<&[u32]>,
    sink: AudioSink,
) -> Result<(Capture, u32)> {
    ensure!(!device.contains('\0'), "microphone.device contains NUL");
    if let Some(channels) = channels {
        selection(MAX_CHANNELS, Some(channels))?;
    }
    start(Some(device.to_owned()), channels.map(<[u32]>::to_vec), sink)
}

pub fn desktop(exclude: &[String], sink: AudioSink, _exclude_own: bool) -> Result<(Capture, u32)> {
    ensure!(
        exclude.is_empty(),
        "desktop_audio.exclude: per-application exclusions are not supported by Linux sink-monitor capture; use an empty list"
    );
    // No Linux playback exists, so there is no own-process output to exclude.
    start(None, None, sink)
}

fn start(
    device: Option<String>,
    channels: Option<Vec<u32>>,
    sink: AudioSink,
) -> Result<(Capture, u32)> {
    let sink = Arc::new(sink);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("pipewire-audio".into())
        .spawn(move || {
            let mut started = false;
            let result = capture_worker(
                device.as_deref(),
                channels.as_deref(),
                &sink,
                &worker_cancel,
                &tx,
                &mut started,
            );
            if let Err(error) = result {
                if started {
                    if !worker_cancel.load(Ordering::Acquire) {
                        (sink.stopped)(format!("PipeWire capture: {error:#}"));
                    }
                } else {
                    let _ = tx.send(Err(error));
                }
            }
        })
        .context("starting PipeWire capture worker")?;
    let capture = Capture {
        cancel,
        worker: Some(worker),
    };
    // Enumeration and stream startup each have an internal deadline. Dropping
    // on timeout cancels and joins rather than leaving an orphaned capture.
    let count = rx
        .recv_timeout(TIMEOUT * 2 + POLL)
        .context("PipeWire capture startup timed out or worker stopped")??;
    Ok((capture, count))
}

#[derive(Default)]
struct Inventory {
    nodes: BTreeMap<u32, Device>,
    input: Option<String>,
    output: Option<String>,
}

#[derive(Default)]
struct Device {
    id: String,
    name: String,
    model: String,
    manufacturer: String,
    input: bool,
    channels: u32,
    rate: u32,
    position: Option<[u32; spa::param::audio::MAX_CHANNELS]>,
}

impl Device {
    fn update(&mut self, props: &spa::utils::dict::DictRef) {
        for (key, dest) in [
            ("node.name", &mut self.id),
            ("node.description", &mut self.name),
            ("device.product.name", &mut self.model),
            ("device.vendor.name", &mut self.manufacturer),
        ] {
            if let Some(value) = props.get(key) {
                *dest = value.to_owned();
            }
        }
        if self.name.is_empty() {
            self.name = props.get("node.nick").unwrap_or(&self.id).to_owned();
        }
        if let Some(count) = props.get("audio.channels").and_then(|v| v.parse().ok()) {
            self.channels = count;
        }
        if let Some(rate) = props.get("audio.rate").and_then(|v| v.parse().ok()) {
            self.rate = rate;
        }
    }
}

pub fn devices() -> Result<(Vec<Microphone>, Vec<AudioOutput>)> {
    // Registry proxies/listeners are !Send, and are created and destroyed here.
    thread::Builder::new()
        .name("pipewire-devices".into())
        .spawn(|| {
            let inventory = enumerate(&AtomicBool::new(false))?;
            let mut microphones = Vec::new();
            let mut outputs = Vec::new();
            for device in inventory.nodes.into_values() {
                if device.id.is_empty() {
                    continue;
                }
                if device.input {
                    microphones.push(Microphone {
                        default: inventory.input.as_deref() == Some(&device.id),
                        id: device.id,
                        name: device.name,
                        model: device.model,
                        manufacturer: device.manufacturer,
                        // PipeWire converts the graph to our fixed stream rate.
                        sample_rates: vec![f64::from(RATE)],
                        channels: device.channels,
                        channel_names: Vec::new(),
                    });
                } else {
                    outputs.push(AudioOutput {
                        default: inventory.output.as_deref() == Some(&device.id),
                        id: device.id,
                        name: device.name,
                        sample_rate: f64::from(device.rate),
                        channels: device.channels,
                    });
                }
            }
            Ok((microphones, outputs))
        })
        .context("starting PipeWire device enumeration")?
        .join()
        .map_err(|_| anyhow!("PipeWire device worker panicked"))?
}

fn enumerate(cancel: &AtomicBool) -> Result<Inventory> {
    pw::init();
    let deadline = Instant::now() + TIMEOUT;
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let registry = core.get_registry_rc()?;
    let inventory = Rc::new(RefCell::new(Inventory::default()));
    let failure = Rc::new(RefCell::new(None));
    let error = failure.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, _, message| {
            if id == pw::core::PW_ID_CORE {
                *error.borrow_mut() = Some(message.to_owned());
            }
        })
        .register();
    let nodes = Rc::new(RefCell::new(Vec::new()));
    let metadata = Rc::new(RefCell::new(Vec::new()));
    let state = inventory.clone();
    let weak_registry = registry.downgrade();
    let keep_nodes = nodes.clone();
    let keep_metadata = metadata.clone();
    let error = failure.clone();
    let removed = inventory.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            let Some(registry) = weak_registry.upgrade() else {
                return;
            };
            if global.type_ == pw::types::ObjectType::Node {
                let input = match props.get("media.class") {
                    Some("Audio/Source") => true,
                    Some("Audio/Sink") => false,
                    _ => return,
                };
                let mut device = Device {
                    input,
                    ..Device::default()
                };
                device.update(props);
                let id = global.id;
                state.borrow_mut().nodes.insert(id, device);
                let node: pw::node::Node = match registry.bind(global) {
                    Ok(node) => node,
                    Err(err) => {
                        *error.borrow_mut() = Some(err.to_string());
                        return;
                    }
                };
                let info_state = state.clone();
                let param_state = state.clone();
                let listener = node
                    .add_listener_local()
                    .info(move |info| {
                        if let (Some(device), Some(props)) =
                            (info_state.borrow_mut().nodes.get_mut(&id), info.props())
                        {
                            device.update(props);
                        }
                    })
                    .param(move |_, _, _, _, param| {
                        let Some(param) = param else { return };
                        let mut format = AudioInfoRaw::new();
                        if format.parse(param).is_ok()
                            && let Some(device) = param_state.borrow_mut().nodes.get_mut(&id)
                        {
                            if format.channels() > 0 {
                                device.channels = format.channels();
                                device.position = Some(format.position());
                            }
                            if format.rate() > 0 {
                                device.rate = format.rate();
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(spa::param::ParamType::EnumFormat), 0, 1);
                node.enum_params(0, Some(spa::param::ParamType::Format), 0, 1);
                keep_nodes.borrow_mut().push((listener, node));
            } else if global.type_ == pw::types::ObjectType::Metadata
                && props.get("metadata.name") == Some("default")
            {
                let object: pw::metadata::Metadata = match registry.bind(global) {
                    Ok(object) => object,
                    Err(err) => {
                        *error.borrow_mut() = Some(err.to_string());
                        return;
                    }
                };
                let state = state.clone();
                let listener = object
                    .add_listener_local()
                    .property(move |subject, key, _, value| {
                        if subject == 0 {
                            let mut state = state.borrow_mut();
                            match key {
                                Some("default.audio.source") => {
                                    state.input = value.and_then(default_name)
                                }
                                Some("default.audio.sink") => {
                                    state.output = value.and_then(default_name)
                                }
                                None => {
                                    state.input = None;
                                    state.output = None;
                                }
                                _ => {}
                            }
                        }
                        0
                    })
                    .register();
                keep_metadata.borrow_mut().push((listener, object));
            }
        })
        .global_remove(move |id| {
            removed.borrow_mut().nodes.remove(&id);
        })
        .register();
    // The first barrier discovers globals. The second covers the bindings,
    // node format queries and default metadata subscribed from those callbacks.
    for _ in 0..2 {
        let done = Rc::new(Cell::new(false));
        let completed = done.clone();
        let pending = core.sync(0)?;
        let _sync = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    completed.set(true);
                }
            })
            .register();
        while !done.get() {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "PipeWire enumeration cancelled"
            );
            ensure!(
                Instant::now() < deadline,
                "PipeWire device enumeration timed out"
            );
            if let Some(error) = failure.borrow_mut().take() {
                bail!("PipeWire enumeration: {error}");
            }
            iterate(&mainloop)?;
        }
    }
    if let Some(error) = failure.borrow_mut().take() {
        bail!("PipeWire enumeration: {error}");
    }
    let result = std::mem::take(&mut *inventory.borrow_mut());
    Ok(result)
}

fn default_name(value: &str) -> Option<String> {
    // PipeWire's own SPA JSON parser handles escaped node names; avoid a
    // hand-written JSON subset or another dependency just for default metadata.
    let value = CString::new(value).ok()?;
    // SAFETY: the string is terminated and lives through this copying call.
    let raw = unsafe { pw::sys::pw_properties_new_string(value.as_ptr()) };
    let raw = std::ptr::NonNull::new(raw)?;
    // SAFETY: new_string returned a uniquely owned pw_properties allocation.
    let props = unsafe { pw::properties::PropertiesBox::from_raw(raw) };
    props.get("name").map(str::to_owned)
}

fn iterate(mainloop: &pw::main_loop::MainLoopRc) -> Result<()> {
    let result = mainloop.loop_().iterate(pw::loop_::Timeout::Finite(POLL));
    ensure!(
        result >= 0 || result == -libc::EINTR,
        "PipeWire loop failed: {result}"
    );
    Ok(())
}

#[derive(Default)]
struct Status {
    ready: Option<u32>,
    error: Option<String>,
}

struct StreamData {
    sink: Arc<AudioSink>,
    status: Rc<RefCell<Status>>,
    channels: u32,
    pick: Vec<usize>,
    samples: Vec<f32>,
    concealed_samples: u64,
    corrupted_chunks: u64,
    clock_offset: i64,
}

impl Drop for StreamData {
    fn drop(&mut self) {
        // One summary per capture, never a per-buffer log on the callback.
        if self.concealed_samples > 0 || self.corrupted_chunks > 0 {
            isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!(
                    "replaced {} non-finite samples with silence; skipped {} corrupted chunks",
                    self.concealed_samples, self.corrupted_chunks
                ),
            );
        }
    }
}

fn capture_worker(
    device: Option<&str>,
    channels: Option<&[u32]>,
    sink: &Arc<AudioSink>,
    cancel: &AtomicBool,
    ready: &mpsc::SyncSender<Result<u32>>,
    started: &mut bool,
) -> Result<()> {
    require_buffer_timestamps()?;
    let inventory = enumerate(cancel)?;
    let microphone = device.is_some();
    let target = if let Some(device) = device {
        let id = if matches!(device, "default" | "main") {
            inventory
                .input
                .as_deref()
                .context("microphone.device: no default PipeWire source")?
        } else {
            device
        };
        Some(
            inventory
                .nodes
                .values()
                .find(|node| node.input && (node.id == id || node.name == id))
                .with_context(|| {
                    format!("microphone.device: {id:?} not found; use a node.name from devices")
                })?,
        )
    } else {
        None
    };
    let count = target.map_or(2, |node| node.channels);
    let pick = selection(count, channels)?;
    let mut props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "media.role" => "Production",
        "node.name" => "isocast.capture",
        "node.description" => "Isocast capture",
    };
    if let Some(target) = target {
        props.insert("target.object", target.id.as_str());
        // Never switch a recording to another input when this device vanishes.
        props.insert("node.dont-reconnect", "true");
        // Capture native channels in their original order, not a stereo remix.
        props.insert("stream.dont-remix", "true");
    } else {
        // With no target.object the session manager selects the default sink.
        props.insert("stream.capture.sink", "true");
    }
    let deadline = Instant::now() + TIMEOUT;
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("connecting audio stream to PipeWire")?;
    let status = Rc::new(RefCell::new(Status::default()));
    let errors = status.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| {
            errors.borrow_mut().error = Some(message.to_owned());
        })
        .register();
    let stream = pw::stream::StreamBox::new(&core, "Isocast audio capture", props)?;
    let clock_offset = clock_offset()?;
    // Share only the Send sink, never a PipeWire object.
    let data = StreamData {
        sink: sink.clone(),
        status: status.clone(),
        channels: 0,
        pick,
        samples: Vec::new(),
        concealed_samples: 0,
        corrupted_chunks: 0,
        clock_offset,
    };
    let _listener = stream.add_local_listener_with_user_data(data)
        .state_changed(|_, data, _, state| match state {
            StreamState::Error(error) => data.status.borrow_mut().error = Some(error),
            StreamState::Unconnected => data.status.borrow_mut().error = Some("audio stream disconnected".into()),
            _ => {}
        })
        .param_changed(move |_, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() { return; }
            let Some(param) = param else { data.channels = 0; return; };
            let mut format = AudioInfoRaw::new();
            if format.parse(param).is_err() || format.format() != AudioFormat::F32LE
                || format.rate() != RATE || format.channels() != count
            {
                data.status.borrow_mut().error = Some(format!(
                    "audio.format: expected interleaved F32LE/{RATE} Hz/{count} channels, got {format:?}"
                ));
                return;
            }
            data.channels = count;
        })
        .process(|stream, data| {
            if data.channels == 0 || data.status.borrow().error.is_some() { return; }
            if let Err(error) = process(stream, data) {
                data.status.borrow_mut().error = Some(format!("{error:#}"));
            }
        }).register()?;
    let mut format = AudioInfoRaw::new();
    format.set_format(AudioFormat::F32LE);
    format.set_rate(RATE);
    format.set_channels(count);
    // Preserve the device's native positions/order, without stereo remixing.
    if let Some(position) = target.and_then(|node| node.position) {
        format.set_position(position);
    } else if !microphone {
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        format.set_position(position);
    }
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
    // No RT_PROCESS: callbacks and the engine sink run on this owned main-loop
    // worker, not PipeWire's realtime thread. This also keeps Rc state confined.
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [pod],
    )?;
    while !cancel.load(Ordering::Acquire) {
        iterate(&mainloop)?;
        let mut state = status.borrow_mut();
        if let Some(error) = state.error.take() {
            bail!("{error}");
        }
        if !*started {
            if let Some(count) = state.ready {
                ready
                    .send(Ok(count))
                    .context("PipeWire startup caller disconnected")?;
                *started = true;
            } else {
                ensure!(
                    Instant::now() < deadline,
                    "PipeWire audio stream produced no samples within 5 seconds; check the session manager and selected device"
                );
            }
        }
    }
    Ok(())
}

// A raw buffer is needed for pw_buffer.time (not exposed by Buffer in 0.10.1).
// The guard returns it on every path, including conversion/timestamp errors.
struct Buffer<'a> {
    stream: &'a pw::stream::Stream,
    raw: std::ptr::NonNull<pw::sys::pw_buffer>,
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        // SAFETY: exactly one buffer dequeued from this still-live stream;
        // neither the guard nor its buffer is shared or returned twice.
        unsafe { self.stream.queue_raw_buffer(self.raw.as_ptr()) };
    }
}

fn process(stream: &pw::stream::Stream, state: &mut StreamData) -> Result<()> {
    // Bound each dispatch even if the graph keeps filling while we drain it,
    // so cancellation and error handling cannot be starved by capture traffic.
    for _ in 0..64 {
        // SAFETY: called on the stream's process/main-loop thread, with no
        // concurrent dequeuer. The guard below immediately takes ownership.
        let Some(raw) = std::ptr::NonNull::new(unsafe { stream.dequeue_raw_buffer() }) else {
            break;
        };
        let buffer = Buffer { stream, raw };
        // SAFETY: PipeWire owns the live buffer until this guard queues it.
        let raw = unsafe { buffer.raw.as_ref() };
        // SAFETY: buffer is a PipeWire-provided spa_buffer or null; no pointer
        // comes from media bytes. as_ref rejects null before dereferencing.
        let spa_buffer =
            unsafe { raw.buffer.as_ref() }.context("audio.buffer: missing SPA buffer")?;
        ensure!(
            spa_buffer.n_datas == 1,
            "audio.buffer: expected one interleaved data plane"
        );
        // SAFETY: n_datas is one and this allocation belongs to the dequeued
        // buffer; null is checked before accessing it.
        let data =
            unsafe { spa_buffer.datas.as_ref() }.context("audio.buffer: missing data plane")?;
        // SAFETY: chunk is owned by this live data plane; reject a null chunk.
        let chunk = unsafe { data.chunk.as_ref() }.context("audio.buffer: missing chunk")?;
        if chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
            state.corrupted_chunks = state.corrupted_chunks.saturating_add(1);
            continue;
        }
        if chunk.size == 0 {
            continue;
        }
        ensure!(
            data.maxsize as usize <= MAX_BUFFER_BYTES,
            "audio.buffer: exceeds 16 MiB limit"
        );
        ensure!(!data.data.is_null(), "audio.buffer: samples are not mapped");
        ensure!(
            data.type_ == spa::sys::SPA_DATA_MemPtr || data.type_ == spa::sys::SPA_DATA_MemFd,
            "audio.buffer: unsupported data memory type {}",
            data.type_
        );
        // SAFETY: MAP_BUFFERS supplies a mapping of maxsize bytes, bounded
        // above; we only read it while the buffer is dequeued.
        let bytes =
            unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), data.maxsize as usize) };
        let concealed = convert(
            bytes,
            chunk.offset as usize,
            chunk.size as usize,
            chunk.stride,
            state.channels,
            &state.pick,
            &mut state.samples,
        )?;
        state.concealed_samples = state.concealed_samples.saturating_add(concealed);
        if state.samples.is_empty() {
            continue;
        }
        let timing = stream.time().context("reading PipeWire audio clock")?;
        let rate = timing.rate();
        ensure!(
            raw.time > 0 && rate.num > 0 && rate.denom > 0,
            "audio.timestamp: PipeWire supplied an invalid graph clock"
        );
        // pw_buffer.time is this buffer's graph-cycle time, even when several
        // cycles queued before this non-RT callback. Subtract the capture path
        // and resampler delay; do not subtract callback latency a second time.
        let timestamp = timestamp(
            raw.time,
            state.clock_offset,
            timing.delay(),
            rate.num,
            rate.denom,
            timing.buffered(),
        )?;
        (state.sink.samples)(&state.samples, timestamp);
        state.status.borrow_mut().ready = Some(state.pick.len() as u32);
    }
    Ok(())
}

fn require_buffer_timestamps() -> Result<()> {
    // The time field was appended to pw_buffer in 1.0.5. Check the loaded
    // library before accessing it, even if built with more recent headers.
    // SAFETY: PipeWire returns a static, NUL-terminated library-version string.
    let version =
        unsafe { std::ffi::CStr::from_ptr(pw::sys::pw_get_library_version()) }.to_string_lossy();
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let version_number = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    ensure!(
        version_number >= (1, 0, 5),
        "PipeWire audio capture requires libpipewire >= 1.0.5 for per-buffer timestamps; loaded {version}"
    );
    Ok(())
}

fn selection(count: u32, channels: Option<&[u32]>) -> Result<Vec<usize>> {
    ensure!(
        (1..=MAX_CHANNELS).contains(&count),
        "microphone.channels: device reports {count} channels; supported range is 1..={MAX_CHANNELS}"
    );
    let chosen = channels.map_or_else(|| (1..=count.min(2)).collect::<Vec<_>>(), <[u32]>::to_vec);
    ensure!(
        !chosen.is_empty()
            && chosen.len() <= 2
            && chosen.iter().all(|channel| (1..=count).contains(channel))
            && (chosen.len() != 2 || chosen[0] != chosen[1]),
        "microphone.channels: select one or two distinct 1-based channels in 1..={count}"
    );
    Ok(chosen
        .into_iter()
        .map(|channel| (channel - 1) as usize)
        .collect())
}

fn convert(
    bytes: &[u8],
    offset: usize,
    size: usize,
    stride: i32,
    channels: u32,
    pick: &[usize],
    output: &mut Vec<f32>,
) -> Result<u64> {
    ensure!(
        (1..=MAX_CHANNELS).contains(&channels),
        "audio.buffer: invalid channel count"
    );
    let frame_bytes = channels as usize * std::mem::size_of::<f32>();
    ensure!(
        stride == 0 || stride == frame_bytes as i32,
        "audio.buffer: invalid interleaved stride {stride}"
    );
    ensure!(
        size <= MAX_BUFFER_BYTES && size.is_multiple_of(frame_bytes),
        "audio.buffer: partial frame or oversized chunk"
    );
    ensure!(!bytes.is_empty(), "audio.buffer: empty mapped data");
    // SPA chunk offsets are relative to maxsize; a wrapped chunk still must
    // fit contiguously in the mapping after normalizing its starting offset.
    let offset = offset % bytes.len();
    let end = offset
        .checked_add(size)
        .context("audio.buffer: chunk offset overflow")?;
    let bytes = bytes
        .get(offset..end)
        .context("audio.buffer: chunk exceeds mapped data")?;
    ensure!(
        !pick.is_empty() && pick.len() <= 2 && pick.iter().all(|&c| c < channels as usize),
        "audio.buffer: invalid selected channels"
    );
    output.clear();
    output.reserve(size / frame_bytes * pick.len());
    let mut concealed = 0;
    for frame in bytes.chunks_exact(frame_bytes) {
        for &channel in pick {
            let start = channel * 4;
            let value = f32::from_le_bytes([
                frame[start],
                frame[start + 1],
                frame[start + 2],
                frame[start + 3],
            ]);
            if value.is_finite() {
                output.push(value);
            } else {
                output.push(0.0);
                concealed += 1;
            }
        }
    }
    Ok(concealed)
}

fn clock_offset() -> Result<i64> {
    // Pick the narrowest of three brackets to minimize scheduling skew. Both
    // clocks are monotonic, but system::now has its own lazy process origin.
    let mut best = (i64::MAX, 0);
    for _ in 0..3 {
        let before = crate::system::now().as_nanos();
        let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: valid output storage; CLOCK_MONOTONIC matches PipeWire's clock.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading PipeWire host clock");
        }
        // SAFETY: successful clock_gettime initialized the timespec.
        let ts = unsafe { ts.assume_init() };
        let after = crate::system::now().as_nanos();
        let monotonic = i128::from(ts.tv_sec) * 1_000_000_000 + i128::from(ts.tv_nsec);
        let offset = i64::try_from(i128::from(before + (after - before) / 2) - monotonic)
            .context("PipeWire clock offset overflow")?;
        if after - before < best.0 {
            best = (after - before, offset);
        }
    }
    Ok(best.1)
}

fn timestamp(
    graph: u64,
    offset: i64,
    delay: i64,
    num: u32,
    denom: u32,
    buffered: u64,
) -> Result<Time> {
    ensure!(
        denom != 0 && num != 0,
        "audio.timestamp: invalid graph rate"
    );
    // Recompute from each absolute graph timestamp; no rounded packet duration
    // is ever accumulated, and missed graph cycles remain visible as gaps.
    let nanos = i128::from(graph) + i128::from(offset)
        - i128::from(delay) * i128::from(num) * 1_000_000_000 / i128::from(denom)
        - i128::from(buffered) * 1_000_000_000 / i128::from(RATE);
    Ok(Time::from_nanos(
        i64::try_from(nanos).context("audio.timestamp: overflow")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Capture>();
    }

    #[test]
    fn channels_are_one_based_distinct_and_preserve_mono() {
        assert_eq!(selection(1, None).unwrap(), [0]);
        assert_eq!(selection(8, None).unwrap(), [0, 1]);
        assert_eq!(selection(8, Some(&[8, 2])).unwrap(), [7, 1]);
        for channels in [&[][..], &[0], &[9], &[1, 1], &[1, 2, 3]] {
            assert!(selection(8, Some(channels)).is_err());
        }
        assert!(selection(0, None).is_err());
        assert!(selection(65, None).is_err());
    }

    #[test]
    fn conversion_honors_chunk_offset_and_channel_order() {
        let mut bytes = vec![0xff; 3]; // deliberately unaligned
        for sample in [1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            bytes.extend(sample.to_le_bytes());
        }
        let mut output = Vec::new();
        convert(&bytes, 3, 24, 12, 3, &[2, 0], &mut output).unwrap();
        assert_eq!(output, [3.0, 1.0, 6.0, 4.0]);
        convert(&bytes, 3, 24, 0, 3, &[1], &mut output).unwrap();
        assert_eq!(output, [2.0, 5.0]);
    }

    #[test]
    fn malformed_chunks_are_rejected_without_indexing_out_of_bounds() {
        let bytes = [0; 16];
        let mut output = Vec::new();
        for (offset, size, stride) in [
            (8, 16, 8),
            (0, 15, 8),
            (0, 16, -8),
            (0, 16, 4),
            (usize::MAX, 8, 8),
        ] {
            assert!(convert(&bytes, offset, size, stride, 2, &[0], &mut output).is_err());
        }
        assert!(convert(&bytes, 0, 16, 8, 2, &[2], &mut output).is_err());
        assert!(convert(&[], 0, 4, 4, 1, &[0], &mut output).is_err());
    }

    #[test]
    fn conversion_conceals_non_finite_samples_and_normalizes_offset() {
        let bytes: Vec<_> = [0.25_f32, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut output = Vec::new();
        assert_eq!(
            convert(
                &bytes,
                bytes.len() * 3,
                bytes.len(),
                4,
                1,
                &[0],
                &mut output
            )
            .unwrap(),
            3
        );
        assert_eq!(output, [0.25, 0.0, 0.0, 0.0, -0.5]);
        assert_eq!(
            convert(&bytes, bytes.len() + 16, 4, 4, 1, &[0], &mut output).unwrap(),
            0
        );
        assert_eq!(output, [-0.5]);
        assert_eq!(convert(&bytes, 0, 8, 8, 2, &[0], &mut output).unwrap(), 0);
        assert_eq!(output, [0.25]);
        assert_eq!(
            convert(&bytes, 0, 8, 8, 2, &[0, 1], &mut output).unwrap(),
            1
        );
        assert_eq!(output, [0.25, 0.0]);
    }

    #[test]
    fn timestamp_maps_engine_origin_and_does_not_accumulate_rounding() {
        let graph = 9_000_000_000_000;
        let offset = -8_999_000_000_000;
        assert_eq!(
            timestamp(graph, offset, 480, 1, RATE, 48)
                .unwrap()
                .as_nanos(),
            989_000_000
        );
        let first = timestamp(graph, offset, 0, 1, RATE, 0).unwrap();
        let hour = timestamp(graph + 3_600_000_000_000, offset, 0, 1, RATE, 0).unwrap();
        assert_eq!(hour.ticks_since(first, RATE), 3_600 * i64::from(RATE));
        assert!(timestamp(graph, offset, 0, 1, 0, 0).is_err());
    }
}
