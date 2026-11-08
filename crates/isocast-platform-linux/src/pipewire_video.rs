//! Portal-authorized PipeWire video, copied from SHM or LINEAR DMA-BUF into bounded BGRA
//! frames. All PipeWire objects and callbacks stay on one owned thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use isocast_engine::{CaptureFormat, FramePool as _, Time, VideoSink};
use pipewire::{self as pw, properties::properties, spa};
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags};
use spa::param::video::{VideoFlags, VideoFormat, VideoInfoRaw};
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

use crate::pw_loop::WorkerLoop;

use crate::frames::{Frame, FramePool};
use crate::tiled;

const START_TIMEOUT: Duration = Duration::from_secs(15);
const LOOP_WAIT: Duration = Duration::from_millis(20);
const MAX_DIMENSION: u32 = 8192;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

pub struct Capture {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The loop polls cancellation even when the producer is silent.
            // Joining also guarantees no sink callbacks survive this handle.
            let _ = thread.join();
        }
    }
}

pub fn start(
    remote: OwnedFd,
    node: u32,
    fps: u32,
    node_removed: impl FnOnce() -> String + Send + 'static,
    sink: VideoSink<Frame>,
) -> Result<(Capture, CaptureFormat)> {
    ensure!(
        node != u32::MAX,
        "PipeWire video node: an explicit portal node is required"
    );
    ensure!(
        (1..=240).contains(&fps),
        "PipeWire video fps: expected 1..=240"
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let (ready, receive) = mpsc::sync_channel(1);
    let worker_cancel = cancel.clone();
    let thread = thread::Builder::new()
        .name("pipewire-video".into())
        .spawn(move || {
            let sink = Arc::new(sink);
            let started = Arc::new(AtomicBool::new(false));
            let result = run(
                remote,
                node,
                fps,
                sink.clone(),
                worker_cancel.clone(),
                started.clone(),
                ready.clone(),
            );
            let reason = match result {
                Ok(End::Cancelled) => return,
                _ if worker_cancel.load(Ordering::Acquire) => return,
                Ok(End::NodeRemoved) => node_removed(),
                Err(error) => format!("PipeWire video: {error:#}"),
            };
            if started.load(Ordering::Acquire) {
                (sink.stopped)(reason);
            } else {
                let _ = ready.try_send(Err(anyhow::anyhow!(reason)));
            }
        })
        .context("starting PipeWire video thread")?;
    let capture = Capture {
        cancel,
        thread: Some(thread),
    };
    // On either error the local handle drops, cancels, and joins. The portal
    // session itself belongs to the caller, not to this capture.
    let format = receive
        .recv_timeout(START_TIMEOUT)
        .context("PipeWire video: no valid mapped frame within 15 seconds")??;
    Ok((capture, format))
}

struct State {
    format: Option<CaptureFormat>,
    /// The stream's own size. `format` keeps the first one, rounded down to
    /// even; a stream of another size is scaled to fit it.
    stream: (u32, u32),
    pixels: VideoFormat,
    /// None while unconfigured, including between two Format events.
    transport: Option<Transport>,
    gpu: Option<(tiled::Importer, tiled::Identity)>,
    images: HashMap<*const spa::sys::spa_data, Arc<tiled::Imported>>,
    fallback: Vec<Vec<u8>>,
    fallback_deadline: Rc<Cell<Option<Instant>>>,
    /// Keyed by the first spa_data's address, not its recyclable fd number.
    mappings: HashMap<*const spa::sys::spa_data, DmaMapping>,
    warned_mismatch: bool,
    warned_stride: bool,
    wait_stats: Option<WaitStats>,
    pool: Option<FramePool>,
    sink: Arc<VideoSink<Frame>>,
    cancel: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<CaptureFormat>>,
    failure: Rc<RefCell<Option<String>>>,
    startup: Rc<RefCell<String>>,
}

enum End {
    Cancelled,
    NodeRemoved,
}

#[derive(Clone, Copy, PartialEq)]
enum Transport {
    Shm,
    Linear,
    Tiled(u64),
}

/// Opt-in hardware qualification. Measure explicit producer/GPU waits only,
/// not memcpy, format conversion or an idle PipeWire graph. SHM has no fence.
#[derive(Default)]
struct WaitStats {
    frames: u32,
    total: Duration,
    max: Duration,
    transport: Option<Transport>,
}

impl WaitStats {
    fn record(&mut self, transport: Transport, elapsed: Duration) {
        if self.transport != Some(transport) {
            *self = Self {
                transport: Some(transport),
                ..Default::default()
            };
        }
        self.frames += 1;
        self.total += elapsed;
        self.max = self.max.max(elapsed);
        if self.frames == 60 {
            let name = match transport {
                Transport::Shm => "shm",
                Transport::Linear => "linear",
                Transport::Tiled(_) => "tiled",
            };
            isocast_diagnostics::diagnostic!("info"; "PipeWire frame waits: transport={name} frames={} total_ns={} max_ns={}",self.frames,self.total.as_nanos(),self.max.as_nanos());
            *self = Self::default();
        }
    }
}

pub(super) fn timed<T>(elapsed: &mut Option<Duration>, f: impl FnOnce() -> T) -> T {
    let Some(total) = elapsed else {
        return f();
    };
    let start = Instant::now();
    let result = f();
    *total += start.elapsed();
    result
}

/// Older portals do not report DRM device IDs. On hybrid systems this means
/// we cannot safely propose any tiled format before seeing a buffer.
fn advertised_devices(
    core: &pw::core::CoreRc,
    registry: &pw::registry::RegistryRc,
    worker: &WorkerLoop,
    target: u32,
) -> Result<Vec<u64>> {
    let ids = Rc::new(RefCell::new(Ok(Vec::new())));
    let found = ids.clone();
    let keep = Rc::new(RefCell::new(None));
    let bound = keep.clone();
    let weak = registry.downgrade();
    let _globals = registry
        .add_listener_local()
        .global(move |global| {
            if global.id != target
                || global.type_ != pw::types::ObjectType::Node
                || !global
                    .permissions
                    .contains(pw::permissions::PermissionFlags::X)
            {
                return;
            }
            let Some(node) = weak
                .upgrade()
                .and_then(|r| r.bind::<pw::node::Node, _>(global).ok())
            else {
                return;
            };
            let seen = found.clone();
            let l = node
                .add_listener_local()
                .param(move |_, _, _, _, pod| {
                    let Some(pod) = pod else { return };
                    let mut result = seen.borrow_mut();
                    if let Ok(ids) = &mut *result {
                        match tiled::device_id(pod) {
                            Ok(Some(id)) => ids.push(id),
                            Ok(None) => (),
                            Err(error) => *result = Err(error),
                        }
                    }
                })
                .register();
            node.enum_params(0, Some(spa::param::ParamType::EnumFormat), 0, 256);
            *bound.borrow_mut() = Some((l, node));
        })
        .register();
    // First discover the node, then wait for the format queries it sent.
    let deadline = Instant::now() + Duration::from_secs(1);
    for _ in 0..2 {
        let done = Rc::new(Cell::new(false));
        let flag = done.clone();
        let seq = core.sync(0)?;
        let _barrier = core
            .add_listener_local()
            .done(move |id, s| {
                if id == pw::core::PW_ID_CORE && s == seq {
                    flag.set(true);
                }
            })
            .register();
        while !done.get() {
            ensure!(
                Instant::now() < deadline,
                "portal node format query timed out"
            );
            worker.iterate(LOOP_WAIT)?;
        }
    }
    ensure!(
        keep.borrow().is_some(),
        "portal node unavailable for format probe"
    );
    ids.replace(Ok(Vec::new()))
}

fn run(
    remote: OwnedFd,
    node: u32,
    fps: u32,
    sink: Arc<VideoSink<Frame>>,
    cancel: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<CaptureFormat>>,
) -> Result<End> {
    pw::sys::require_supported().map_err(anyhow::Error::msg)?;
    pw::init();
    let mainloop = WorkerLoop::new()?;
    let context = pw::context::ContextRc::new(mainloop.main_loop(), None)?;
    // PipeWire takes ownership and closes the fd on both connect failure and
    // disconnect. No raw-fd duplicate or second owner is retained here.
    let core = context.connect_fd_rc(remote, None)?;
    let failure = Rc::new(RefCell::new(None));
    let startup = Rc::new(RefCell::new("no format or buffers received".into()));
    let deadline = Instant::now() + START_TIMEOUT - Duration::from_millis(100);
    let core_failure = failure.clone();
    let probing = Rc::new(Cell::new(false));
    let probe_error = Rc::new(Cell::new(false));
    let (querying, query_failed) = (probing.clone(), probe_error.clone());
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, code, message| {
            // Discovering the optional GPU identity must not break SHM on a
            // portal that permits linking to a node but refuses EnumFormat.
            if querying.get() && id != pw::core::PW_ID_CORE {
                query_failed.set(true);
                return;
            }
            core_failure
                .borrow_mut()
                .get_or_insert_with(|| format!("remote error on object {id} ({code}): {message}"));
        })
        .register();
    // A vanished portal node (the portal restarted, or dropped the stream
    // without closing the session) sends no portal `Closed`: report it here
    // rather than rely on the session manager destroying our node.
    let registry = core.get_registry_rc()?;
    let node_removed = Rc::new(Cell::new(false));
    let removed = node_removed.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global_remove(move |id| {
            if id == node {
                removed.set(true);
            }
        })
        .register();
    let stream = mainloop.stream(pw::stream::StreamBox::new(
        &core,
        "Isocast screen capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            "node.name" => "isocast-screen-capture",
            // With DONT_RECONNECT at connect: bind to the portal's node
            // only, never falling back, moving or relinking to another
            // source when it goes away.
            "node.dont-fallback" => "true",
            "node.dont-move" => "true",
        },
    )?);
    let gpu = tiled::Importer::probe().and_then(|gpu| {
        probing.set(true);
        let ids = advertised_devices(&core, &registry, &mainloop, node);
        probing.set(false);
        if probe_error.get() {
            return None;
        }
        let ids = ids.inspect_err(|error| isocast_diagnostics::diagnostic!("info"; "PipeWire tiled offer withheld: {error:#}")).ok()?;
        let Some(identity) = gpu.identity(&ids) else {
            isocast_diagnostics::diagnostic!("info"; "PipeWire tiled offer withheld: no verified exporter GPU (advertised DRM device ids: {ids:?})");
            return None;
        };
        Some((gpu, identity))
    });
    let fallback = fallback_offers(fps)?;
    // No driver's tiled implementation has passed the hardware A/B gate.
    // SHM, then LINEAR, remain preferred even when Vulkan can import tiles.
    let mut offers = fallback.clone();
    if let Some((gpu, id)) = &gpu {
        for pixels in [
            VideoFormat::BGRA,
            VideoFormat::BGRx,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
        ] {
            if let Some(offer) = tiled_offer(fps, pixels, gpu.modifiers(pixels), *id) {
                offers.push(serialize(offer)?);
            }
        }
    }
    let fallback_deadline = Rc::new(Cell::new(None));
    let state = State {
        format: None,
        stream: (0, 0),
        pixels: VideoFormat::BGRA,
        transport: None,
        gpu,
        images: HashMap::new(),
        fallback,
        fallback_deadline: fallback_deadline.clone(),
        mappings: HashMap::new(),
        warned_mismatch: false,
        warned_stride: false,
        wait_stats: (std::env::var("ISOCAST_CAPTURE_WAIT_TIMINGS").as_deref() == Ok("1"))
            .then(WaitStats::default),
        pool: None,
        sink,
        cancel: cancel.clone(),
        started: started.clone(),
        ready,
        failure: failure.clone(),
        startup: startup.clone(),
    };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(|_, state, old, new| {
            let reason = match new {
                pw::stream::StreamState::Error(reason) => Some(reason),
                pw::stream::StreamState::Unconnected
                    if old != pw::stream::StreamState::Unconnected =>
                {
                    Some("screen stream disconnected".into())
                }
                _ => None,
            };
            if let Some(reason) = reason {
                state.failure.borrow_mut().get_or_insert(reason);
            }
        })
        .param_changed(move |stream, state, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            state.transport = None;
            state.mappings.clear();
            state.images.clear();
            let Some(param) = param else { return };
            if let Err(error) = configure(stream, state, param, fps) {
                state
                    .failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("negotiating screen format: {error:#}"));
            }
        })
        .remove_buffer(|_, state, raw| {
            // SAFETY: PipeWire supplies the live pw_buffer and spa_buffer in
            // this callback, before it can recycle the plane or close its fd.
            if let Some(buffer) = unsafe { raw.as_ref().and_then(|p| p.buffer.as_ref()) } {
                state.mappings.remove(&buffer.datas.cast_const());
                state.images.remove(&buffer.datas.cast_const());
            }
        })
        .process(|stream, state| {
            if state.cancel.load(Ordering::Acquire) || state.failure.borrow().is_some() {
                return;
            }
            if let Err(error) = catch_unwind(AssertUnwindSafe(|| process(stream, state)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("screen frame callback panicked")))
            {
                state
                    .failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("reading screen frame: {error:#}"));
            }
        })
        .register()?;
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::DONT_RECONNECT,
        &mut pods(&offers)?,
    )?;
    while !cancel.load(Ordering::Acquire) {
        if node_removed.get() {
            return Ok(End::NodeRemoved);
        }
        ensure!(
            started.load(Ordering::Acquire) || Instant::now() < deadline,
            "no valid screen frame: {}",
            startup.borrow()
        );
        if let Some(reason) = failure.borrow_mut().take() {
            bail!(reason);
        }
        ensure!(
            !fallback_deadline
                .get()
                .is_some_and(|at| Instant::now() >= at),
            "producer did not deliver a fallback screen frame within 5 seconds"
        );
        mainloop.iterate(LOOP_WAIT)?;
    }
    // Locals drop in reverse order; the stream disconnects and flushes its
    // queued callbacks on this thread.
    Ok(End::Cancelled)
}

fn configure(stream: &pw::stream::Stream, state: &mut State, pod: &Pod, fps: u32) -> Result<()> {
    let (kind, subtype) = spa::param::format_utils::parse_format(pod)?;
    ensure!(
        kind == spa::param::format::MediaType::Video
            && subtype == spa::param::format::MediaSubtype::Raw,
        "expected raw video"
    );
    let mut raw = VideoInfoRaw::new();
    // An optional SPA long of the wrong type is left untouched by parse.
    raw.set_modifier(u64::MAX);
    raw.parse(pod)?;
    let dma = raw.flags().contains(VideoFlags::MODIFIER);
    // The allocating peer may still be choosing a modifier. It publishes a
    // fixed Format before buffers arrive; never interpret an unfixed plane.
    if raw.flags().bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED != 0 {
        return Ok(());
    }
    ensure!(
        matches!(
            raw.format(),
            VideoFormat::BGRA
                | VideoFormat::BGRx
                | VideoFormat::RGBA
                | VideoFormat::RGBx
                | VideoFormat::RGB
                | VideoFormat::BGR
        ),
        "pixel format: expected BGRA, BGRx, RGBA, RGBx, RGB or BGR; got {:?}",
        raw.format()
    );
    let size = raw.size();
    ensure!(
        (1..=MAX_DIMENSION).contains(&size.width) && (1..=MAX_DIMENSION).contains(&size.height),
        "size: expected 1..=8192 on each axis; got {}x{}",
        size.width,
        size.height
    );
    let transport = match (dma, raw.modifier()) {
        (false, _) => Transport::Shm,
        (true, DRM_FORMAT_MOD_LINEAR) => Transport::Linear,
        (true, m) => {
            let (gpu, identity) = state.gpu.as_ref().context("unexpected tiled format")?;
            let selected = match tiled::device_id(pod) {
                Ok(selected) => selected,
                Err(error) => return fallback(stream, state, &error.to_string()),
            };
            if let tiled::Identity::Matched(id) = *identity
                && selected != Some(id)
            {
                return fallback(
                    stream,
                    state,
                    &format!(
                        "expected DRM device id {id}, negotiated {selected:?} for modifier {m:#018x}"
                    ),
                );
            }
            if let Some(id) = selected
                && !gpu.same_device(id)
            {
                return fallback(
                    stream,
                    state,
                    &format!(
                        "negotiated DRM device id {id} is not the engine GPU for modifier {m:#018x}"
                    ),
                );
            }
            if !gpu
                .modifiers(raw.format())
                .iter()
                .any(|p| p.value == m && size.width <= p.width && size.height <= p.height)
            {
                return fallback(
                    stream,
                    state,
                    &format!(
                        "Vulkan did not offer {:?} {}x{} with modifier {m:#018x}",
                        raw.format(),
                        size.width,
                        size.height
                    ),
                );
            }
            isocast_diagnostics::diagnostic!("info"; "PipeWire negotiated {:?} {}x{} DRM modifier {m:#018x}",raw.format(),size.width,size.height);
            Transport::Tiled(m)
        }
    };
    // Desktop streams may report 0/1 (variable rate). The engine records at
    // the requested rate; actual delivery timestamps remain independent.
    // The portal shares windows at any size, but H.264's 4:2:0 needs even
    // ones: drop the last column or row of an odd size. A stream that
    // changes size later (a resized window, a screen's new mode) keeps the
    // first size, and `process` scales its frames to fit.
    if state.format.is_none() {
        let format = CaptureFormat {
            width: even(size.width),
            height: even(size.height),
            fps,
        };
        state.pool = Some(FramePool::new(format.width, format.height)?);
        state.format = Some(format);
    }
    state.stream = (size.width, size.height);
    state.pixels = raw.format();
    *state.startup.borrow_mut() = format!(
        "negotiated {}x{} {:?}; no mapped buffer received",
        size.width,
        size.height,
        raw.format()
    );
    let types = if dma {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemPtr) | (1 << spa::sys::SPA_DATA_MemFd)
    };
    let buffers = serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: types,
                    flags: vec![types],
                },
            ))),
        )],
    })?;
    let header = serialize(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamMeta,
        id: spa::sys::SPA_PARAM_Meta,
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    })?;
    stream.update_params(&mut [
        Pod::from_bytes(&buffers).context("invalid buffer pod")?,
        Pod::from_bytes(&header).context("invalid metadata pod")?,
    ])?;
    state.transport = Some(transport);
    Ok(())
}

/// `size` rounded down to an even number, at least 2, as H.264's 4:2:0
/// needs.
pub(crate) fn even(size: u32) -> u32 {
    (size & !1).max(2)
}

/// The bytes in one pixel of a negotiated packed format.
fn pixel_bytes(format: VideoFormat) -> Option<usize> {
    match format {
        VideoFormat::RGB | VideoFormat::BGR => Some(3),
        VideoFormat::BGRA | VideoFormat::BGRx | VideoFormat::RGBA | VideoFormat::RGBx => Some(4),
        _ => None,
    }
}

/// A chunk's row stride. A chunk that gives none (0) is read as tightly
/// packed rows of the stream's `size` only when its byte size is exactly
/// that; otherwise the stride stays 0 and `Layout::new` drops the chunk.
fn chunk_stride(stride: i32, bytes: u32, (width, height): (u32, u32), format: VideoFormat) -> i32 {
    if stride != 0 {
        return stride;
    }
    let row = pixel_bytes(format).and_then(|pixel| (width as usize).checked_mul(pixel));
    match row {
        Some(row) if row.checked_mul(height as usize) == Some(bytes as usize) => {
            i32::try_from(row).unwrap_or(0)
        }
        _ => 0,
    }
}

/// Whether a stream of `stream` size copies straight into `format`: the
/// same size, or one more column or row that is dropped.
fn fits((width, height): (u32, u32), format: CaptureFormat) -> bool {
    (format.width..=format.width + 1).contains(&width)
        && (format.height..=format.height + 1).contains(&height)
}

fn process(stream: &pw::stream::Stream, state: &mut State) -> Result<()> {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return Ok(());
    };
    let (Some(format), Some(transport)) = (state.format, state.transport) else {
        return Ok(());
    };
    // Keep the requeue-on-Drop guard *outside* the unwind boundary. A
    // panicking backend may have submitted the copy without returning a fence.
    let result = catch_unwind(AssertUnwindSafe(|| {
        read_frame(stream, state, &mut buffer, format, transport)
    }));
    if let Some((gpu, _)) = state.gpu.as_mut()
        && gpu.in_flight()
    {
        // Stream teardown reclaims this slot. Never hand it to the producer
        // as a completed frame, or free Vulkan's untracked image/staging now.
        std::mem::forget(buffer);
        tiled::disable_after_fault();
        gpu.retire();
        return match result {
            Ok(Err(error)) => {
                Err(error
                    .context("tiled fence unconfirmed; buffer withheld, disabling tiled capture"))
            }
            _ => Err(anyhow::anyhow!(
                "tiled submission did not complete; buffer withheld, disabling tiled capture"
            )),
        };
    }
    drop(buffer);
    let frame = match result {
        Ok(Ok(frame)) => frame,
        Ok(Err(error)) if matches!(transport, Transport::Tiled(_)) => {
            return fallback(stream, state, &format!("{error:#}"));
        }
        Ok(Err(error)) => return Err(error),
        Err(_) if matches!(transport, Transport::Tiled(_)) => {
            tiled::disable_after_fault();
            return fallback(
                stream,
                state,
                "tiled frame callback panicked; disabling tiled capture",
            );
        }
        Err(_) => bail!("screen frame callback panicked"),
    };
    if let Some((frame, timestamp)) = frame {
        state.fallback_deadline.set(None);
        if state.cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        if !state.started.load(Ordering::Acquire) {
            state.started.store(true, Ordering::Release);
            if state.ready.try_send(Ok(format)).is_err() {
                state.cancel.store(true, Ordering::Release);
                return Ok(());
            }
        }
        (state.sink.frame)(frame, timestamp);
    }
    Ok(())
}

fn read_frame(
    stream: &pw::stream::Stream,
    state: &mut State,
    buffer: &mut pw::buffer::Buffer<'_>,
    format: CaptureFormat,
    transport: Transport,
) -> Result<Option<(Frame, Time)>> {
    let mut waited = state.wait_stats.as_ref().map(|_| Duration::ZERO);
    let header = buffer.find_meta::<MetaHeader>();
    if !state.started.load(Ordering::Acquire) {
        *state.startup.borrow_mut() = format!(
            "received buffer; header flags={:?}",
            header.map(MetaHeader::flags)
        );
    }
    if header.is_some_and(|header| {
        header
            .flags()
            .intersects(MetaHeaderFlags::CORRUPTED | MetaHeaderFlags::GAP)
    }) {
        return Ok(None);
    }
    let pts = header.map(MetaHeader::pts);
    let timestamp = frame_time(pts, stream.time().ok().map(|time| time.now()));
    let data = buffer.datas_mut();
    // Only single-plane packed RGB was negotiated. Empty/corrupt frames are
    // not fatal (compositors can send them while a surface is hidden).
    if data.len() != 1 {
        if !std::mem::replace(&mut state.warned_mismatch, true) {
            isocast_diagnostics::diagnostic!("warn"; "screen capture: dropping mismatched buffer (expected one packed plane, got {})", data.len());
        }
        return Ok(None);
    }
    let data = data[0].as_raw();
    if !state.started.load(Ordering::Acquire) {
        *state.startup.borrow_mut() = format!(
            "buffer type={} flags={} mapped={} maxsize={}",
            data.type_,
            data.flags,
            !data.data.is_null(),
            data.maxsize
        );
    }
    // Input buffers mapped by PipeWire are readable. portal-wlr 0.7.1 leaves
    // spa_data.flags at NONE even for its valid mapped MemFd buffers; requiring
    // the optional READABLE flag would silently discard every screen frame.
    // SAFETY: PipeWire owns the dequeued buffer and its chunk until requeue.
    // Unlike Data::chunk(), as_ref handles a missing chunk without panicking.
    let Some(chunk) = (unsafe { data.chunk.as_ref() }) else {
        return Ok(None);
    };
    if !state.started.load(Ordering::Acquire) {
        *state.startup.borrow_mut() = format!(
            "buffer maxsize={} offset={} size={} stride={} flags={}",
            data.maxsize, chunk.offset, chunk.size, chunk.stride, chunk.flags
        );
    }
    if chunk.flags & 3 != 0 {
        // SPA_CHUNK_FLAG_CORRUPTED | SPA_CHUNK_FLAG_EMPTY
        return Ok(None);
    }
    let dma = !matches!(transport, Transport::Shm);
    let stride = if dma {
        chunk.stride
    } else {
        chunk_stride(chunk.stride, chunk.size, state.stream, state.pixels)
    };
    if stride == 0 {
        if !std::mem::replace(&mut state.warned_stride, true) {
            isocast_diagnostics::diagnostic!("warn"; "screen capture: dropping buffer with unspecified row stride and no provable packed layout");
        }
        return Ok(None);
    }
    let direct = fits(state.stream, format);
    let (width, height) = if direct {
        (format.width, format.height)
    } else {
        state.stream
    };
    let pixels = state.pixels;
    let pool = state
        .pool
        .as_mut()
        .context("missing negotiated frame pool")?;
    let mut draw = |source: &[u8], layout: &Layout| {
        pool.draw(&mut |target, _| {
            if direct {
                copy_bgra(source, target, layout, pixels);
            } else {
                scale_bgra(
                    source,
                    target,
                    layout,
                    pixels,
                    (format.width, format.height),
                );
            }
        })
    };
    let frame = if dma && data.type_ == spa::sys::SPA_DATA_DmaBuf {
        let fd = i32::try_from(data.fd).context("invalid DMA-BUF fd")?;
        ensure!(fd >= 0, "missing DMA-BUF fd");
        // SAFETY: PipeWire owns this fd until the borrowed buffer is requeued.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let offset = usize::try_from(u64::from(data.mapoffset) + u64::from(chunk.offset))?;
        if let Transport::Tiled(modifier) = transport {
            timed(&mut waited, || tiled::wait_writer(fd))
                .context("DMA-BUF acquire fence unavailable")?;
            let (gpu, identity) = state.gpu.as_mut().context("no tiled importer")?;
            let key = std::ptr::from_ref(data);
            let plane = tiled::Plane {
                fd: fd.as_raw_fd(),
                modifier,
                offset: offset as u64,
                stride: u64::try_from(stride).context("negative tiled row stride")?,
            };
            let dynamic = data.flags & spa::sys::SPA_DATA_FLAG_DYNAMIC != 0;
            if dynamic
                || state
                    .images
                    .get(&key)
                    .is_none_or(|image| image.plane != plane)
            {
                ensure!(
                    gpu.matches_fd(fd, *identity),
                    "DMA-BUF exporter does not match the compositor GPU"
                );
                let image = gpu.import(fd, plane, pixels, state.stream)?;
                state.images.insert(key, Arc::new(image));
            }
            let layout = Layout::packed(0, (state.stream.0 * 4) as i32, width, height, pixels)
                .context("invalid tiled readback size")?;
            let frame = gpu.read(&state.images[&key], state.stream, &mut waited, |bytes| {
                draw(bytes, &layout)
            })??;
            if dynamic {
                state.images.remove(&key);
            }
            frame
        } else {
            let Some(layout) = Layout::packed(offset, stride, width, height, pixels) else {
                return Ok(None);
            };
            let key = std::ptr::from_ref(data);
            // A dynamic plane may replace the allocation even with the same fd
            // number. Never retain a mapping of that plane across dequeues.
            let transient;
            let mapping = if data.flags & spa::sys::SPA_DATA_FLAG_DYNAMIC != 0 {
                state.mappings.remove(&key);
                transient = DmaMapping::map(fd, layout.end)?;
                &transient
            } else {
                if state
                    .mappings
                    .get(&key)
                    .is_none_or(|m| m.fd != fd.as_raw_fd() || m.len < layout.end)
                {
                    state.mappings.insert(key, DmaMapping::map(fd, layout.end)?);
                }
                &state.mappings[&key]
            };
            let mut read = timed(&mut waited, || mapping.begin_read(fd))?;
            let frame = draw(read.bytes(), &layout);
            timed(&mut waited, || read.end())?;
            frame?
        }
    } else if !dma
        && matches!(
            data.type_,
            spa::sys::SPA_DATA_MemPtr | spa::sys::SPA_DATA_MemFd
        )
        && !data.data.is_null()
    {
        // MAP_BUFFERS already adjusts the SHM pointer for data.mapoffset.
        let Some(layout) = Layout::new(
            data.maxsize,
            chunk.offset,
            chunk.size,
            stride,
            width,
            height,
            pixels,
        ) else {
            return Ok(None);
        };
        // SAFETY: The checked rows fit within the mapped SHM chunk.
        draw(
            unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), layout.end) },
            &layout,
        )?
    } else {
        if !std::mem::replace(&mut state.warned_mismatch, true) {
            isocast_diagnostics::diagnostic!("warn"; "screen capture: dropping buffer of type {} (mapped={}) after negotiating {}", data.type_, !data.data.is_null(), if dma { "DMA-BUF" } else { "SHM" });
        }
        return Ok(None);
    };
    if frame.is_some()
        && let (Some(stats), Some(waited)) = (&mut state.wait_stats, waited)
    {
        stats.record(transport, waited);
    }
    Ok(frame.map(|f| (f, timestamp)))
}

// Offset is the start of the valid memory chunk, not the first logical scan
// line. A negative stride describes a bottom-up image in that same chunk.
struct Layout {
    offset: usize,
    stride: usize,
    row: usize,
    output_row: usize,
    pixel_bytes: usize,
    height: usize,
    bottom_up: bool,
    end: usize,
}

impl Layout {
    fn new(
        maxsize: u32,
        offset: u32,
        size: u32,
        stride: i32,
        width: u32,
        height: u32,
        format: VideoFormat,
    ) -> Option<Self> {
        if maxsize == 0 || size > maxsize {
            return None;
        }
        // SPA offsets in a SHM chunk are modulo maxsize; reject wrapped rows.
        let offset = (offset % maxsize) as usize;
        let layout = Self::packed(offset, stride, width, height, format)?;
        (layout.end - offset <= size as usize && layout.end <= maxsize as usize).then_some(layout)
    }

    fn packed(
        offset: usize,
        stride: i32,
        width: u32,
        height: u32,
        format: VideoFormat,
    ) -> Option<Self> {
        if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
            return None;
        }
        let pixel_bytes = pixel_bytes(format)?;
        let row = (width as usize).checked_mul(pixel_bytes)?;
        let output_row = (width as usize).checked_mul(4)?;
        let step = stride.unsigned_abs() as usize;
        if step < row {
            return None;
        }
        let span = step.checked_mul(height as usize - 1)?.checked_add(row)?;
        let end = offset.checked_add(span)?;
        if end > isize::MAX as usize {
            return None;
        }
        Some(Self {
            offset,
            stride: step,
            row,
            output_row,
            pixel_bytes,
            height: height as usize,
            bottom_up: stride < 0,
            end,
        })
    }
}

/// A mapping can outlive a dequeue, but CPU access must not.
struct DmaMapping {
    fd: i32, // identity check only; borrowed from PipeWire for each syscall
    ptr: *mut libc::c_void,
    len: usize,
}

impl DmaMapping {
    fn map(fd: BorrowedFd<'_>, len: usize) -> Result<Self> {
        // SPA permits DMA-BUF maxsize and chunk.size to be zero or one.
        // SAFETY: SEEK_END with offset zero is the DMA-BUF size query.
        let allocation = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        if allocation < 0 {
            return Err(std::io::Error::last_os_error()).context("query DMA-BUF size");
        }
        ensure!(
            len as u64 <= allocation as u64,
            "DMA-BUF rows exceed allocation"
        );
        // SAFETY: len is nonzero and within the kernel-reported allocation.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap LINEAR DMA-BUF");
        }
        Ok(Self {
            fd: fd.as_raw_fd(),
            ptr,
            len,
        })
    }

    fn begin_read<'a>(&'a self, fd: BorrowedFd<'a>) -> Result<DmaRead<'a>> {
        let mut read = DmaRead {
            mapping: self,
            fd,
            active: false,
        };
        read.sync(1).context("DMA_BUF_SYNC_START | READ")?;
        read.active = true;
        Ok(read)
    }
}

impl Drop for DmaMapping {
    fn drop(&mut self) {
        // SAFETY: this object owns the mapping; no read guard can outlive it.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

struct DmaRead<'a> {
    mapping: &'a DmaMapping,
    fd: BorrowedFd<'a>,
    active: bool,
}

impl DmaRead<'_> {
    fn sync(&self, flags: u64) -> std::io::Result<()> {
        // linux/dma-buf.h: _IOW('b', 0, struct dma_buf_sync { __u64 flags; })
        loop {
            // SAFETY: fd lives through the ioctl, whose only input is one u64.
            if unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    libc::_IOW::<u64>(b'b'.into(), 0),
                    &flags,
                )
            } == 0
            {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if !matches!(err.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
                return Err(err);
            }
        }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: map succeeds and START completes before exposing any bytes.
        unsafe { std::slice::from_raw_parts(self.mapping.ptr.cast(), self.mapping.len) }
    }

    fn end(&mut self) -> Result<()> {
        self.active = false;
        self.sync(1 | 4).context("DMA_BUF_SYNC_END | READ")
    }
}

impl Drop for DmaRead<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.end();
        }
    }
}

/// Turns one pixel of `format` into BGRA.
fn to_bgra(format: VideoFormat) -> impl Fn(&[u8]) -> [u8; 4] {
    let swap = matches!(
        format,
        VideoFormat::RGBA | VideoFormat::RGBx | VideoFormat::RGB
    );
    let opaque = matches!(
        format,
        VideoFormat::BGRx | VideoFormat::RGBx | VideoFormat::RGB | VideoFormat::BGR
    );
    move |src| {
        [
            src[if swap { 2 } else { 0 }],
            src[1],
            src[if swap { 0 } else { 2 }],
            if opaque { 255 } else { src[3] },
        ]
    }
}

/// The start of source row `y` (counted from the top) in `layout`.
fn row_start(layout: &Layout, y: usize) -> usize {
    let y = if layout.bottom_up {
        layout.height - 1 - y
    } else {
        y
    };
    layout.offset + y * layout.stride
}

fn copy_bgra(source: &[u8], target: &mut [u8], layout: &Layout, format: VideoFormat) {
    let convert = to_bgra(format);
    for (y, output) in target
        .chunks_exact_mut(layout.output_row)
        .take(layout.height)
        .enumerate()
    {
        let start = row_start(layout, y);
        let input = &source[start..start + layout.row];
        if format == VideoFormat::BGRA {
            output.copy_from_slice(input);
        } else {
            for (src, dst) in input
                .chunks_exact(layout.pixel_bytes)
                .zip(output.as_chunks_mut::<4>().0)
            {
                *dst = convert(src);
            }
        }
    }
}

/// Copy a frame of another size into `target`, a `size` BGRA frame: scaled
/// to fit, keeping its shape, centred on black (nearest pixel).
fn scale_bgra(
    source: &[u8],
    target: &mut [u8],
    layout: &Layout,
    format: VideoFormat,
    size: (u32, u32),
) {
    let convert = to_bgra(format);
    let (in_width, in_height) = (layout.row / layout.pixel_bytes, layout.height);
    let (out_width, out_height) = (size.0 as usize, size.1 as usize);
    let (width, height) = if in_width * out_height >= in_height * out_width {
        (
            out_width,
            (in_height * out_width / in_width).clamp(1, out_height),
        )
    } else {
        (
            (in_width * out_height / in_height).clamp(1, out_width),
            out_height,
        )
    };
    let (left, top) = ((out_width - width) / 2, (out_height - height) / 2);
    // Byte offset in an input row of each output column.
    let columns: Vec<usize> = (0..width)
        .map(|x| x * in_width / width * layout.pixel_bytes)
        .collect();
    for (y, output) in target
        .chunks_exact_mut(out_width * 4)
        .take(out_height)
        .enumerate()
    {
        let output = output.as_chunks_mut::<4>().0;
        output.fill([0, 0, 0, 255]);
        if !(top..top + height).contains(&y) {
            continue;
        }
        let start = row_start(layout, (y - top) * in_height / height);
        let input = &source[start..start + layout.row];
        for (dst, &at) in output[left..left + width].iter_mut().zip(&columns) {
            *dst = convert(&input[at..at + layout.pixel_bytes]);
        }
    }
}

fn monotonic_ns() -> Option<i64> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes one timespec on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the successful call initialized time.
    let time = unsafe { time.assume_init() };
    i64::try_from(i128::from(time.tv_sec) * 1_000_000_000 + i128::from(time.tv_nsec)).ok()
}

fn frame_time(pts: Option<i64>, stream_now: Option<i64>) -> Time {
    // Bracket the engine clock with native-clock samples. The two clocks have
    // different origins; raw PipeWire PTS must NEVER reach VideoSink.
    let before = monotonic_ns();
    let engine = crate::system::now();
    let after = monotonic_ns();
    let (Some(before), Some(after)) = (before, after) else {
        return engine;
    };
    let native = before.saturating_add(after.saturating_sub(before) / 2);
    map_time(engine, native, pts, stream_now)
}

fn map_time(engine: Time, native: i64, pts: Option<i64>, stream_now: Option<i64>) -> Time {
    // Some producers use a stream-relative PTS or SPA_TIME_INVALID. In that
    // case use the graph cycle's documented CLOCK_MONOTONIC timestamp, not an
    // invented offset from the first received frame (which includes latency).
    let valid = |time: &i64| *time > 0 && time.abs_diff(native) <= 10_000_000_000;
    let timestamp = pts
        .filter(valid)
        .or_else(|| stream_now.filter(valid))
        .unwrap_or(native);
    Time::from_nanos(
        engine
            .as_nanos()
            .saturating_add(timestamp.saturating_sub(native)),
    )
}

fn serialize(object: Object) -> Result<Vec<u8>> {
    Ok(spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map_err(|error| anyhow::anyhow!("serializing PipeWire pod: {error:?}"))?
    .0
    .into_inner())
}

fn fallback(stream: &pw::stream::Stream, state: &mut State, why: &str) -> Result<()> {
    state
        .fallback_deadline
        .set(Some(Instant::now() + Duration::from_secs(5)));
    state.transport = None;
    state.images.clear();
    state.gpu = None;
    isocast_diagnostics::diagnostic!("info"; "tiled screen capture unavailable: {why}; re-offering SHM and LINEAR");
    stream.update_params(&mut pods(&state.fallback)?)?;
    Ok(())
}

fn pods(offers: &[Vec<u8>]) -> Result<Vec<&Pod>> {
    offers
        .iter()
        .map(|o| Pod::from_bytes(o).context("invalid screen format pod"))
        .collect()
}
fn modifier_prop(m: u64) -> Property {
    Property {
        key: spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: spa::pod::PropertyFlags::MANDATORY,
        value: Value::Long(m as i64),
    }
}
fn fallback_offers(fps: u32) -> Result<Vec<Vec<u8>>> {
    let mut linear = format_offer(fps);
    linear.properties.push(modifier_prop(DRM_FORMAT_MOD_LINEAR));
    Ok(vec![serialize(format_offer(fps))?, serialize(linear)?])
}

fn tiled_offer(
    fps: u32,
    pixels: VideoFormat,
    modifiers: &[tiled::Modifier],
    identity: tiled::Identity,
) -> Option<Object> {
    use spa::utils::Rectangle;
    let first = modifiers.first()?;
    let mut offer = format_offer(fps);
    for p in &mut offer.properties {
        if p.key == spa::sys::SPA_FORMAT_VIDEO_format {
            p.value = Value::Id(spa::utils::Id(pixels.as_raw()));
        }
        if p.key == spa::sys::SPA_FORMAT_VIDEO_size {
            let max = Rectangle {
                width: modifiers
                    .iter()
                    .map(|m| m.width)
                    .fold(MAX_DIMENSION, u32::min),
                height: modifiers
                    .iter()
                    .map(|m| m.height)
                    .fold(MAX_DIMENSION, u32::min),
            };
            p.value = Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Rectangle {
                        width: 1920.min(max.width),
                        height: 1080.min(max.height),
                    },
                    min: Rectangle {
                        width: 1,
                        height: 1,
                    },
                    max,
                },
            )));
        }
    }
    // Pass the allocator the actual intersection as a choice, not a series
    // of pre-fixated guesses. It will announce the modifier it allocated.
    offer.properties.push(Property {
        key: spa::sys::SPA_FORMAT_VIDEO_modifier,
        flags: spa::pod::PropertyFlags::MANDATORY | spa::pod::PropertyFlags::DONT_FIXATE,
        value: Value::Choice(ChoiceValue::Long(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Enum {
                default: first.value as i64,
                alternatives: modifiers.iter().map(|m| m.value as i64).collect(),
            },
        ))),
    });
    if let tiled::Identity::Matched(id) = identity {
        offer.properties.push(Property {
            key: tiled::DEVICE_ID,
            flags: spa::pod::PropertyFlags::MANDATORY,
            value: Value::Bytes(id.to_ne_bytes().to_vec()),
        });
    }
    Some(offer)
}

fn format_offer(fps: u32) -> Object {
    use spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    use spa::pod::{object, property};
    use spa::utils::{Fraction, Rectangle, SpaTypes};
    object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(F::MediaType, Id, MediaType::Video),
        property!(F::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            F::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRA,
            VideoFormat::BGRA,
            VideoFormat::BGRx,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            // NVIDIA's wlroots SHM readback can be packed RGB even when its
            // DMA-BUF offer is BGRx. Keep a mapped-memory path for that format.
            VideoFormat::RGB,
            VideoFormat::BGR
        ),
        property!(
            F::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: MAX_DIMENSION,
                height: MAX_DIMENSION
            }
        ),
        property!(
            F::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: fps, denom: 1 }
        ),
        property!(
            F::VideoMaxFramerate,
            Fraction,
            Fraction { num: fps, denom: 1 }
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_pipewire_format_preserves_the_exporter_device_id() -> Result<()> {
        // xdpw's fixed T4 format: filter the producer and consumer PODs with
        // SPA itself. Reading the two *offers* alone misses the Choice wrapper.
        let id = libc::makedev(226, 128);
        let modifier = tiled::Modifier {
            value: 0x0300_0000_0060_6014,
            width: 1920,
            height: 1080,
        };
        let offer = tiled_offer(
            60,
            VideoFormat::BGRx,
            &[modifier],
            tiled::Identity::Matched(id),
        )
        .unwrap();
        let mut producer = offer.clone();
        for p in &mut producer.properties {
            if p.key == spa::sys::SPA_FORMAT_VIDEO_modifier {
                *p = modifier_prop(modifier.value);
            }
            if p.key == spa::sys::SPA_FORMAT_VIDEO_size {
                p.value = Value::Rectangle(spa::utils::Rectangle {
                    width: 1920,
                    height: 1080,
                });
            }
        }
        let bytes = serialize(producer)?;
        let producer = Pod::from_bytes(&bytes).unwrap();
        assert_eq!(tiled::device_id(producer)?, Some(id));
        let bytes = serialize(offer.clone())?;
        let consumer = Pod::from_bytes(&bytes).unwrap();
        let mut storage = vec![0; 2048];
        let builder = spa::pod::builder::Builder::new(&mut storage);
        let mut filtered = std::ptr::null_mut();
        // SAFETY: the inputs live through filtering. The builder owns the
        // output backing store until after all reads of the fixated object.
        let pod = unsafe {
            assert_eq!(
                spa::sys::spa_pod_filter(
                    builder.as_raw_ptr(),
                    &mut filtered,
                    producer.as_raw_ptr(),
                    consumer.as_raw_ptr()
                ),
                0
            );
            assert!(!filtered.is_null());
            assert_eq!(spa::sys::spa_pod_fixate(filtered), 0);
            Pod::from_raw(filtered)
        };
        let mut raw = VideoInfoRaw::new();
        raw.parse(pod)?;
        assert_eq!(
            (
                raw.format(),
                raw.modifier(),
                raw.size().width,
                raw.size().height
            ),
            (VideoFormat::BGRx, modifier.value, 1920, 1080)
        );
        assert_eq!(tiled::device_id(pod)?, Some(id));
        for value in [
            Value::Long(id as i64),
            Value::Bytes(vec![0; 4]),
            Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: id as i64,
                    alternatives: vec![id as i64, 1],
                },
            ))),
        ] {
            let mut bad = offer.clone();
            bad.properties
                .iter_mut()
                .find(|p| p.key == tiled::DEVICE_ID)
                .unwrap()
                .value = value;
            let bytes = serialize(bad)?;
            assert!(tiled::device_id(Pod::from_bytes(&bytes).unwrap()).is_err());
        }
        Ok(())
    }

    #[test]
    fn tiled_pod_binds_the_exact_format_modifier_and_exporter() -> Result<()> {
        let modifier = tiled::Modifier {
            value: 0x0300000000000010,
            width: 1024,
            height: 512,
        };
        let other = tiled::Modifier {
            value: modifier.value + 1,
            ..modifier
        };
        assert!(tiled_offer(60, VideoFormat::BGRx, &[], tiled::Identity::Single).is_none());
        let offer = tiled_offer(
            60,
            VideoFormat::BGRx,
            &[modifier, other],
            tiled::Identity::Matched(13),
        )
        .unwrap();
        let prop = offer
            .properties
            .iter()
            .find(|p| p.key == spa::sys::SPA_FORMAT_VIDEO_modifier)
            .unwrap();
        assert!(
            matches!(&prop.value,Value::Choice(ChoiceValue::Long(Choice(_,ChoiceEnum::Enum{alternatives,..}))) if alternatives==&[modifier.value as i64,other.value as i64])
        );
        let bytes = serialize(offer)?;
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut raw = VideoInfoRaw::new();
        raw.parse(pod)?;
        assert_eq!(raw.format(), VideoFormat::BGRx);
        assert!(raw.flags().contains(VideoFlags::MODIFIER));
        assert_ne!(
            raw.flags().bits() & spa::sys::SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED,
            0
        );
        assert_eq!(tiled::device_id(pod)?, Some(13));
        let fallback = fallback_offers(60)?;
        let pods = pods(&fallback)?;
        raw.parse(pods[0])?;
        assert!(!raw.flags().contains(VideoFlags::MODIFIER));
        raw.parse(pods[1])?;
        assert!(raw.flags().contains(VideoFlags::MODIFIER));
        assert_eq!(raw.modifier(), 0);
        Ok(())
    }

    #[test]
    #[ignore = "requires the standalone LINEAR-only PipeWire producer"]
    fn linear_dmabuf_only_pixels() -> Result<()> {
        let node = std::env::var("ISOCAST_DMABUF_NODE")?.parse()?;
        let path = std::path::PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").context("no runtime dir")?,
        )
        .join("pipewire-0");
        let remote = std::os::unix::net::UnixStream::connect(path)?;
        let (send, receive) = mpsc::sync_channel(1);
        let (capture, format) = start(
            remote.into(),
            node,
            30,
            || "producer disappeared".into(),
            VideoSink {
                frame: Box::new(move |frame, _| {
                    let _ = send.try_send(frame);
                }),
                stopped: Box::new(|reason| eprintln!("{reason}")),
            },
        )?;
        assert_eq!((format.width, format.height), (64, 32));
        let expected: Vec<_> = (0..32u8)
            .flat_map(|y| (0..64u8).flat_map(move |x| [x, y, x ^ y, 255]))
            .collect();
        for _ in 0..3 {
            let frame = receive.recv_timeout(Duration::from_secs(2))?;
            assert_eq!(frame.read_bgra()?, expected);
        }
        let mut swapped = expected.clone();
        for pixel in swapped.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut new_frames = 0;
        while new_frames < 3 {
            let frame = receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            let bytes = frame.read_bgra()?;
            if bytes == swapped {
                new_frames += 1;
            } else {
                assert_eq!(new_frames, 0, "format reverted after renegotiation");
                assert_eq!(bytes, expected);
            }
        }
        drop(capture);
        eprintln!(
            "LINEAR DMA-BUF: matched all 8192 bytes per frame before and after BGRA → RGBA renegotiation"
        );
        Ok(())
    }

    #[test]
    fn handle_is_send() {
        fn send<T: Send>() {}
        send::<Capture>();
    }

    #[test]
    fn maps_native_timestamps_to_engine_origin_and_rejects_invalid_pts() {
        let engine = Time::from_nanos(3_000_000_000);
        let native = 900_000_000_000;
        let cycle = native - 10_000_000;
        assert_eq!(
            map_time(engine, native, Some(native - 20_000_000), Some(cycle)).as_nanos(),
            2_980_000_000
        );
        for pts in [
            None,
            Some(i64::MIN),
            Some(0),
            Some(1_000_000_000),
            Some(i64::MAX),
        ] {
            assert_eq!(
                map_time(engine, native, pts, Some(cycle)).as_nanos(),
                2_990_000_000
            );
        }
        assert_eq!(map_time(engine, native, None, None), engine);
    }

    #[test]
    fn copies_offsets_padding_and_bottom_up_rows() {
        let source = [99, 99, 1, 2, 3, 4, 88, 88, 5, 6, 7, 8];
        for (stride, expected) in [
            (6, [1, 2, 3, 4, 5, 6, 7, 8]),
            (-6, [5, 6, 7, 8, 1, 2, 3, 4]),
        ] {
            let layout = Layout::new(12, 2, 10, stride, 1, 2, VideoFormat::BGRA).unwrap();
            let mut target = [0; 8];
            copy_bgra(&source, &mut target, &layout, VideoFormat::BGRA);
            assert_eq!(target, expected);
        }
    }

    #[test]
    fn odd_streams_copy_into_even_frames() {
        assert_eq!(
            (even(2118), even(1059), even(2), even(1)),
            (2118, 1058, 2, 2)
        );
        let format = |width, height| CaptureFormat {
            width,
            height,
            fps: 30,
        };
        assert!(fits((3, 3), format(2, 2)));
        assert!(fits((2, 2), format(2, 2)));
        assert!(!fits((1, 1), format(2, 2)));
        assert!(!fits((4, 2), format(2, 2)));
        // A 3x3 BGRA stream copies its top-left 2x2.
        let source: Vec<u8> = (0..36).collect();
        let layout = Layout::new(36, 0, 36, 12, even(3), even(3), VideoFormat::BGRA).unwrap();
        let mut target = [0; 16];
        copy_bgra(&source, &mut target, &layout, VideoFormat::BGRA);
        assert_eq!(
            target,
            [0, 1, 2, 3, 4, 5, 6, 7, 12, 13, 14, 15, 16, 17, 18, 19]
        );
    }

    #[test]
    fn a_missing_stride_is_trusted_only_for_tightly_packed_chunks() {
        assert_eq!(chunk_stride(-16, 0, (3, 3), VideoFormat::BGRA), -16);
        // 3x3: rows of 12 BGRA bytes or 9 RGB bytes, and nothing else.
        assert_eq!(chunk_stride(0, 36, (3, 3), VideoFormat::BGRA), 12);
        assert_eq!(chunk_stride(0, 27, (3, 3), VideoFormat::RGB), 9);
        // Padded rows, a short chunk, or no size: stays 0, and is dropped.
        for bytes in [48, 24, 0] {
            assert_eq!(chunk_stride(0, bytes, (3, 3), VideoFormat::BGRA), 0);
        }
        assert!(Layout::new(48, 0, 48, 0, 3, 3, VideoFormat::BGRA).is_none());
    }

    #[test]
    fn resized_streams_scale_into_the_first_size_on_black() {
        const B: [u8; 4] = [0, 0, 0, 255];
        // A 2x1 BGRx stream (pixels 1 and 2) into a 4x4 frame: 4x2 in the
        // middle rows, each pixel doubled, black above and below.
        let source = [1, 1, 1, 0, 2, 2, 2, 0];
        let layout = Layout::new(8, 0, 8, 8, 2, 1, VideoFormat::BGRx).unwrap();
        let mut target = [0; 64];
        scale_bgra(&source, &mut target, &layout, VideoFormat::BGRx, (4, 4));
        let one = [1, 1, 1, 255];
        let two = [2, 2, 2, 255];
        let row = [one, one, two, two];
        let rows: Vec<[[u8; 4]; 4]> = target
            .as_chunks::<16>()
            .0
            .iter()
            .map(|r| *r.as_chunks::<4>().0.first_chunk().unwrap())
            .collect();
        assert_eq!(rows, [[B; 4], row, row, [B; 4]]);
        // A 1x2 bottom-up stream into 4x2: one column each side is black.
        let source = [7, 7, 7, 0, 9, 9, 9, 0];
        let layout = Layout::new(8, 0, 8, -4, 1, 2, VideoFormat::BGRx).unwrap();
        let mut target = [0; 32];
        scale_bgra(&source, &mut target, &layout, VideoFormat::BGRx, (4, 2));
        let nine = [9, 9, 9, 255];
        let seven = [7, 7, 7, 255];
        assert_eq!(target.as_chunks::<4>().0, [B, nine, B, B, B, seven, B, B]);
    }

    #[test]
    fn converts_all_four_formats_and_fills_x_alpha() {
        let layout = Layout::new(4, 0, 4, 4, 1, 1, VideoFormat::BGRA).unwrap();
        for (format, expected) in [
            (VideoFormat::BGRA, [1, 2, 3, 4]),
            (VideoFormat::BGRx, [1, 2, 3, 255]),
            (VideoFormat::RGBA, [3, 2, 1, 4]),
            (VideoFormat::RGBx, [3, 2, 1, 255]),
        ] {
            let mut target = [0; 4];
            copy_bgra(&[1, 2, 3, 4], &mut target, &layout, format);
            assert_eq!(target, expected);
        }
    }

    #[test]
    fn expands_packed_rgb_with_offsets_padding_and_bottom_up_rows() {
        let source = [99, 99, 1, 2, 3, 4, 5, 6, 88, 88, 7, 8, 9, 10, 11, 12];
        for format in [VideoFormat::RGB, VideoFormat::BGR] {
            for stride in [8, -8] {
                let layout = Layout::new(16, 2, 14, stride, 2, 2, format).unwrap();
                let mut target = [0; 16];
                copy_bgra(&source, &mut target, &layout, format);
                let mut expected = if format == VideoFormat::RGB {
                    [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
                } else {
                    [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
                };
                if stride < 0 {
                    expected.rotate_left(8);
                }
                assert_eq!(target, expected);
                assert!(Layout::new(16, 2, 13, stride, 2, 2, format).is_none());
                assert!(Layout::new(15, 2, 14, stride, 2, 2, format).is_none());
                assert!(Layout::new(16, 2, 14, 5, 2, 2, format).is_none());
            }
        }
    }

    #[test]
    fn rejects_truncated_wrapped_and_overflowing_layouts() {
        for args in [
            (0, 0, 0, 4, 1, 1),
            (8, 0, 8, 4, 0, 1),
            (8, 0, 8, 4, 1, 0),
            (8, 0, 8, 0, 1, 2),
            (8, 0, 8, 3, 1, 2),
            (8, 0, 7, 4, 1, 2),
            (8, 1, 8, 4, 1, 2),
            (8, 0, 9, 4, 1, 2),
            (8, 0, 8, i32::MIN, 1, 2),
            (8, 0, 8, i32::MAX, 1, 2),
            (u32::MAX, 0, u32::MAX, i32::MAX, 8192, 8192),
            (u32::MAX, 0, u32::MAX, 4, u32::MAX, u32::MAX),
        ] {
            assert!(
                Layout::new(
                    args.0,
                    args.1,
                    args.2,
                    args.3,
                    args.4,
                    args.5,
                    VideoFormat::BGRA
                )
                .is_none()
            );
        }
        assert!(Layout::new(8, 8, 8, 4, 1, 2, VideoFormat::BGRA).is_some());
        assert!(Layout::new(8, 0, 8, 4, 1, 2, VideoFormat::NV12).is_none());
    }
}
