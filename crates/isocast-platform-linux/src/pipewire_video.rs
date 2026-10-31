//! Portal-authorized PipeWire video, copied from mapped SHM into bounded BGRA
//! frames. All PipeWire objects and callbacks stay on one owned thread.

use std::cell::RefCell;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use isocast_engine::{CaptureFormat, FramePool as _, Time, VideoSink};
use pipewire::{self as pw, properties::properties, spa};
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

use crate::pw_loop::WorkerLoop;

use crate::frames::{Frame, FramePool};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const LOOP_WAIT: Duration = Duration::from_millis(20);
const MAX_DIMENSION: u32 = 8192;

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
            if let Err(error) = result {
                let reason = format!("PipeWire video: {error:#}");
                if started.load(Ordering::Acquire) {
                    if !worker_cancel.load(Ordering::Acquire) {
                        (sink.stopped)(reason);
                    }
                } else {
                    let _ = ready.try_send(Err(anyhow::anyhow!(reason)));
                }
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
    pool: Option<FramePool>,
    sink: Arc<VideoSink<Frame>>,
    cancel: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<CaptureFormat>>,
    failure: Rc<RefCell<Option<String>>>,
    startup: Rc<RefCell<String>>,
}

fn run(
    remote: OwnedFd,
    node: u32,
    fps: u32,
    sink: Arc<VideoSink<Frame>>,
    cancel: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<CaptureFormat>>,
) -> Result<()> {
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
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, code, message| {
            core_failure
                .borrow_mut()
                .get_or_insert_with(|| format!("remote error on object {id} ({code}): {message}"));
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
        },
    )?);
    let state = State {
        format: None,
        stream: (0, 0),
        pixels: VideoFormat::BGRA,
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
            let Some(param) = param else { return };
            if let Err(error) = configure(stream, state, param, fps) {
                state
                    .failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("negotiating screen format: {error:#}"));
            }
        })
        .process(|stream, state| {
            if state.cancel.load(Ordering::Acquire) || state.failure.borrow().is_some() {
                return;
            }
            if let Err(error) = process(stream, state) {
                state
                    .failure
                    .borrow_mut()
                    .get_or_insert_with(|| format!("reading screen frame: {error:#}"));
            }
        })
        .register()?;
    let format = serialize(format_offer(fps))?;
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [Pod::from_bytes(&format).context("invalid video format pod")?],
    )?;
    while !cancel.load(Ordering::Acquire) {
        ensure!(
            started.load(Ordering::Acquire) || Instant::now() < deadline,
            "no valid screen frame: {}",
            startup.borrow()
        );
        if let Some(reason) = failure.borrow_mut().take() {
            bail!(reason);
        }
        mainloop.iterate(LOOP_WAIT)?;
    }
    // Locals drop in order: listener, stream (disconnected and its queued
    // callbacks flushed on this thread), core listener, core, context, loop.
    Ok(())
}

fn configure(stream: &pw::stream::Stream, state: &mut State, pod: &Pod, fps: u32) -> Result<()> {
    let (kind, subtype) = spa::param::format_utils::parse_format(pod)?;
    ensure!(
        kind == spa::param::format::MediaType::Video
            && subtype == spa::param::format::MediaSubtype::Raw,
        "expected raw video"
    );
    let mut raw = VideoInfoRaw::new();
    raw.parse(pod)?;
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
    // Explicitly exclude DMA-BUF, even when the compositor offers it first.
    // MAP_BUFFERS maps MemFd and adjusts data for mapoffset; do not add that
    // file offset again when reading the mapped pointer.
    let types = (1 << spa::sys::SPA_DATA_MemPtr) | (1 << spa::sys::SPA_DATA_MemFd);
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
    let Some(format) = state.format else {
        return Ok(());
    };
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
        return Ok(());
    }
    let pts = header.map(MetaHeader::pts);
    let timestamp = frame_time(pts, stream.time().ok().map(|time| time.now()));
    let data = buffer.datas_mut();
    // Only single-plane packed RGB was negotiated. Empty/corrupt frames are
    // not fatal (compositors can send them while a surface is hidden).
    if data.len() != 1 {
        return Ok(());
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
    if !matches!(
        data.type_,
        spa::sys::SPA_DATA_MemPtr | spa::sys::SPA_DATA_MemFd
    ) || data.data.is_null()
    {
        return Ok(());
    }
    // Input buffers mapped by PipeWire are readable. portal-wlr 0.7.1 leaves
    // spa_data.flags at NONE even for its valid mapped MemFd buffers; requiring
    // the optional READABLE flag would silently discard every screen frame.
    // SAFETY: PipeWire owns the dequeued buffer and its chunk until requeue.
    // Unlike Data::chunk(), as_ref handles a missing chunk without panicking.
    let Some(chunk) = (unsafe { data.chunk.as_ref() }) else {
        return Ok(());
    };
    if !state.started.load(Ordering::Acquire) {
        *state.startup.borrow_mut() = format!(
            "buffer maxsize={} offset={} size={} stride={} flags={}",
            data.maxsize, chunk.offset, chunk.size, chunk.stride, chunk.flags
        );
    }
    if chunk.flags & 3 != 0 {
        // SPA_CHUNK_FLAG_CORRUPTED | SPA_CHUNK_FLAG_EMPTY
        return Ok(());
    }
    let direct = fits(state.stream, format);
    let (width, height) = if direct {
        (format.width, format.height)
    } else {
        state.stream
    };
    let Some(layout) = Layout::new(
        data.maxsize,
        chunk.offset,
        chunk.size,
        chunk_stride(chunk.stride, chunk.size, state.stream, state.pixels),
        width,
        height,
        state.pixels,
    ) else {
        return Ok(());
    };
    // SAFETY: MAP_BUFFERS supplied a readable mapping of maxsize bytes. The
    // validated layout's end lies within it and within the advertised chunk.
    // The immutable slice neither claims writable access nor outlives buffer.
    let source = unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), layout.end) };
    let pixels = state.pixels;
    let frame = state
        .pool
        .as_mut()
        .context("missing negotiated frame pool")?
        .draw(&mut |target, _| {
            if direct {
                copy_bgra(source, target, &layout, pixels);
            } else {
                scale_bgra(
                    source,
                    target,
                    &layout,
                    pixels,
                    (format.width, format.height),
                );
            }
        })?;
    // Requeue before calling the sink: downstream retains only our own copy.
    drop(buffer);
    if let Some(frame) = frame {
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
        if maxsize == 0
            || size == 0
            || size > maxsize
            || !(1..=MAX_DIMENSION).contains(&width)
            || !(1..=MAX_DIMENSION).contains(&height)
        {
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
        // SPA offsets are modulo maxsize. Wrapped video chunks cannot be
        // represented by a single mapped slice and are rejected below.
        let offset = (offset % maxsize) as usize;
        let end = offset.checked_add(span)?;
        if span > size as usize || end > maxsize as usize || end > isize::MAX as usize {
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
        for (x, dst) in output[left..left + width].iter_mut().enumerate() {
            let at = x * in_width / width * layout.pixel_bytes;
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
