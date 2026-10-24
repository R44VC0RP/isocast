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
            VideoFormat::BGRA | VideoFormat::BGRx | VideoFormat::RGBA | VideoFormat::RGBx
        ),
        "pixel format: expected BGRA, BGRx, RGBA or RGBx; got {:?}",
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
    let format = CaptureFormat {
        width: size.width,
        height: size.height,
        fps,
    };
    if let Some(previous) = state.format {
        ensure!(
            previous == format,
            "screen size changed; restart capture to renegotiate"
        );
    } else {
        state.pool = Some(FramePool::new(format.width, format.height)?);
        state.format = Some(format);
    }
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
    let Some(layout) = Layout::new(
        data.maxsize,
        chunk.offset,
        chunk.size,
        chunk.stride,
        format.width,
        format.height,
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
        .draw(&mut |target, _| copy_bgra(source, target, &layout, pixels))?;
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
    ) -> Option<Self> {
        if maxsize == 0
            || size == 0
            || size > maxsize
            || !(1..=MAX_DIMENSION).contains(&width)
            || !(1..=MAX_DIMENSION).contains(&height)
        {
            return None;
        }
        let row = (width as usize).checked_mul(4)?;
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
            height: height as usize,
            bottom_up: stride < 0,
            end,
        })
    }
}

fn copy_bgra(source: &[u8], target: &mut [u8], layout: &Layout, format: VideoFormat) {
    for (y, output) in target
        .chunks_exact_mut(layout.row)
        .take(layout.height)
        .enumerate()
    {
        let y = if layout.bottom_up {
            layout.height - 1 - y
        } else {
            y
        };
        let start = layout.offset + y * layout.stride;
        let input = &source[start..start + layout.row];
        if format == VideoFormat::BGRA {
            output.copy_from_slice(input);
        } else {
            let swap = format == VideoFormat::RGBA || format == VideoFormat::RGBx;
            let opaque = format == VideoFormat::BGRx || format == VideoFormat::RGBx;
            for (src, dst) in input
                .as_chunks::<4>()
                .0
                .iter()
                .zip(output.as_chunks_mut::<4>().0)
            {
                dst.copy_from_slice(&[
                    src[if swap { 2 } else { 0 }],
                    src[1],
                    src[if swap { 0 } else { 2 }],
                    if opaque { 255 } else { src[3] },
                ]);
            }
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
            VideoFormat::RGBx
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
            let layout = Layout::new(12, 2, 10, stride, 1, 2).unwrap();
            let mut target = [0; 8];
            copy_bgra(&source, &mut target, &layout, VideoFormat::BGRA);
            assert_eq!(target, expected);
        }
    }

    #[test]
    fn converts_all_four_formats_and_fills_x_alpha() {
        let layout = Layout::new(4, 0, 4, 4, 1, 1).unwrap();
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
            assert!(Layout::new(args.0, args.1, args.2, args.3, args.4, args.5).is_none());
        }
        assert!(Layout::new(8, 8, 8, 4, 1, 2).is_some());
    }
}
