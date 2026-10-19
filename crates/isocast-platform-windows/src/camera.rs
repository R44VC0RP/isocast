//! Media Foundation camera samples, kept native as NV12 where possible.
//! Packed YUV/RGB uses GPU conversion after one upload; MJPEG uses the Media
//! Foundation decoder followed by that same upload path. Interleaved fields
//! preserve their existing rows. Busy cameras attach read-only through the
//! Windows Frame Server. No GPU readback, interpolation or timestamp rebasing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{CameraRequest, CaptureFormat, Time, VideoSink};
use isocast_protocol::{CameraCaptureMetadata, Canvas};
use parking_lot::Mutex;
use windows::Foundation::{IClosable, IPropertyValue, TypedEventHandler};
use windows::Graphics::Imaging::{BitmapBufferAccessMode, BitmapPixelFormat};
use windows::Media::Capture::Frames::{
    MediaFrameFormat, MediaFrameReader, MediaFrameReaderStartStatus, MediaFrameReference,
    MediaFrameSourceGroup, MediaFrameSourceKind,
};
use windows::Media::Capture::{
    MediaCapture, MediaCaptureFailedEventHandler, MediaCaptureInitializationSettings,
    MediaCaptureMemoryPreference, MediaCaptureSharingMode, StreamingCaptureMode,
};
use windows::Win32::Foundation::{S_OK, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::Win32::System::WinRT::{IBufferByteAccess, IMemoryBufferByteAccess};
use windows::core::{GUID, HRESULT, Interface, Ref, implement};

use crate::clock;
use crate::frames::{Frame, GraphicsDevice};
use crate::mf::Runtime;
use crate::native::{Apartment, Event};
mod jpeg;
mod pixels;

use jpeg::JpegDecoder;
pub use pixels::CameraPixelFormat;
use pixels::{CameraPixels, Color, Delivery, Layout};

const MAX_DEVICES: u32 = 256;
const MAX_FORMATS: u32 = 512;
const IN_FLIGHT: usize = 8;
const START_WAIT: Duration = Duration::from_secs(5);
const COPY_WAIT: Duration = Duration::from_secs(5);
// A failed copy signal or a hung GPU must not let retries accumulate retained
// camera sources forever. Existing captures also stop requesting new samples.
static QUARANTINED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub media_type_interlace_mode: u32,
    pub pixel_format: CameraPixelFormat,
    /// Exact advertised rate; fps is its nominal integer (e.g.59.94→60).
    pub rate_numerator: u32,
    pub rate_denominator: u32,
}

#[derive(Clone, Debug)]
pub struct CameraDevice {
    /// Exact MF video-capture symbolic link, not the friendly name.
    pub id: String,
    pub name: String,
    /// Supported native SDR formats; actual GPU/CPU delivery is observed later.
    /// Metadata alone does not guarantee that the driver delivers GPU samples.
    pub formats: Vec<CameraFormat>,
}

type Startup = std::result::Result<(CameraDevice, CaptureFormat), String>;

#[derive(Clone, Copy, Default, Debug)]
pub struct CameraCaptureStats {
    /// Samples received by the native callback, including rejected samples.
    pub received_frames: u64,
    /// Frame callbacks that returned normally.
    pub delivered_frames: u64,
    pub pool_drops: u64,
    pub discontinuities: u64,
    pub timestamp_errors: u64,
    pub format_changes: u64,
    /// Timestamp intervals over 1.5 frame periods, NOT an exact loss count.
    pub gap_events: u64,
    pub stream_ticks: u64,
    /// Samples marked interlaced, woven without changing either field's rows.
    pub woven_frames: u64,
    /// Woven samples whose first temporal field is the bottom field.
    pub bottom_first_frames: u64,
}

#[derive(Default)]
struct Counters {
    received_frames: AtomicU64,
    delivered_frames: AtomicU64,
    pool_drops: AtomicU64,
    discontinuities: AtomicU64,
    timestamp_errors: AtomicU64,
    format_changes: AtomicU64,
    gap_events: AtomicU64,
    stream_ticks: AtomicU64,
    woven_frames: AtomicU64,
    bottom_first_frames: AtomicU64,
}

struct State {
    stop: Event,
    wake: Event,
    stopping: AtomicBool,
    mailbox: Mutex<Mailbox>,
    counts: Counters,
    observed: Arc<ObservedFlags>,
}

// Three bits per flag: absent, false, true. Bounded observations can outlive
// capture for a take manifest without retaining samples, events or the GPU.
#[derive(Default)]
struct ObservedFlags {
    media_type_interlace_mode: AtomicU32,
    mode: Mutex<ModeInfo>,
    delivery: AtomicU8,
    interlaced: AtomicU8,
    bottom_first: AtomicU8,
    single_field: AtomicU8,
    repeat_first: AtomicU8,
}

#[derive(Clone, Default)]
struct ModeInfo {
    native_format: String,
    decoded_format: String,
    native_frame_rate: String,
    color: String,
    shared: bool,
}

fn describe_mode(state: &State, native: CameraFormat, layout: Layout, shared: bool) {
    *state.observed.mode.lock() = ModeInfo {
        native_format: native.pixel_format.name().into(),
        decoded_format: if native.pixel_format == CameraPixelFormat::Mjpg {
            layout.format.name().into()
        } else {
            String::new()
        },
        native_frame_rate: format!("{}/{}", native.rate_numerator, native.rate_denominator),
        color: if matches!(
            layout.format,
            CameraPixelFormat::Rgb24 | CameraPixelFormat::Rgb32 | CameraPixelFormat::Argb32
        ) {
            if layout.color.full_range {
                "rgb-full"
            } else {
                "rgb-limited"
            }
            .into()
        } else {
            format!(
                "bt{}-{}",
                layout.color.matrix,
                if layout.color.full_range {
                    "full"
                } else {
                    "limited"
                }
            )
        },
        shared,
    };
}

fn note_delivery(state: &State, delivery: Delivery) {
    let bit = match delivery {
        Delivery::NativeGpu => 1,
        Delivery::NativeUpload => 2,
        Delivery::ConvertedGpu => 4,
        Delivery::ConvertedUpload => 8,
    };
    state.observed.delivery.fetch_or(bit, Ordering::Relaxed);
}

fn observe(flag: &AtomicU8, value: Option<bool>) {
    flag.fetch_or(
        match value {
            None => 1,
            Some(false) => 2,
            Some(true) => 4,
        },
        Ordering::Relaxed,
    );
}

fn observed(flag: &AtomicU8) -> Vec<Option<bool>> {
    let bits = flag.load(Ordering::Relaxed);
    [None, Some(false), Some(true)]
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| (bits & (1 << index) != 0).then_some(value))
        .collect()
}

#[derive(Default)]
struct Mailbox {
    messages: VecDeque<Message>,
    error: Option<String>,
    closed: bool,
}

enum Message {
    Sample {
        status: HRESULT,
        stream: u32,
        flags: u32,
        sample: Option<IMFSample>,
        arrived: Time,
    },
    Event(IMFMediaEvent),
}

// SAFETY: MF callbacks and the capture worker are in the process MTA pinned
// by Apartment. These COM references move only inside that apartment, under
// the mailbox mutex; none is exposed to the caller or a user callback.
unsafe impl Send for Message {}

impl State {
    fn post(&self, message: Message) {
        let mut mailbox = self.mailbox.lock();
        if mailbox.closed || self.stopping.load(Ordering::Acquire) {
            return;
        }
        if mailbox.messages.len() == IN_FLIGHT {
            mailbox
                .error
                .get_or_insert_with(|| "MF camera callback mailbox overflow".into());
        } else {
            mailbox.messages.push_back(message);
        }
        self.wake.signal();
    }

    fn next(&self) -> Result<Option<Message>> {
        let mut mailbox = self.mailbox.lock();
        if let Some(error) = mailbox.error.take() {
            bail!("{error}");
        }
        Ok(mailbox.messages.pop_front())
    }

    fn close(&self) {
        let messages = {
            let mut mailbox = self.mailbox.lock();
            mailbox.closed = true;
            std::mem::take(&mut mailbox.messages)
        };
        // Release native samples/events on the worker without the mailbox
        // lock held: a final COM release may itself run native callbacks.
        drop(messages);
    }

    fn wait(&self, milliseconds: u32) -> Result<()> {
        let result = unsafe {
            WaitForMultipleObjects(
                &[self.stop.handle(), self.wake.handle()],
                false,
                milliseconds,
            )
        };
        if result == WAIT_FAILED {
            return Err(windows::core::Error::from_thread()).context("MF camera wait");
        }
        if result != WAIT_OBJECT_0 && result.0 != WAIT_OBJECT_0.0 + 1 && result != WAIT_TIMEOUT {
            bail!("unexpected MF camera wait result: {}", result.0);
        }
        Ok(())
    }
}

#[implement(IMFSourceReaderCallback)]
struct Callback {
    state: Arc<State>,
}

impl IMFSourceReaderCallback_Impl for Callback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        stream: u32,
        flags: u32,
        _: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        if sample.as_ref().is_some() {
            self.state
                .counts
                .received_frames
                .fetch_add(1, Ordering::Relaxed);
        }
        self.state.post(Message::Sample {
            status,
            stream,
            flags,
            sample: sample.as_ref().cloned(),
            arrived: clock::now(),
        });
        Ok(())
    }

    fn OnEvent(&self, _: u32, event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        if let Some(event) = event.as_ref() {
            self.state.post(Message::Event(event.clone()));
        } else {
            let mut mailbox = self.state.mailbox.lock();
            mailbox
                .error
                .get_or_insert_with(|| "MF camera returned a null event".into());
            self.state.wake.signal();
        }
        Ok(())
    }

    fn OnFlush(&self, _: u32) -> windows::core::Result<()> {
        let mut mailbox = self.state.mailbox.lock();
        if !mailbox.closed && !self.state.stopping.load(Ordering::Acquire) {
            mailbox
                .error
                .get_or_insert_with(|| "MF camera flushed unexpectedly".into());
            self.state.wake.signal();
        }
        Ok(())
    }
}

/// Drop signals and joins the worker, including native shutdown. Native calls
/// and user callbacks can block; there is no hard wall-clock stop guarantee.
/// Self-drop from a sink callback requests stop; cleanup is deferred until
/// that callback returns, rather than attempting to join its own thread.
pub struct CameraCapture {
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
    device: CameraDevice,
}

impl CameraCapture {
    pub fn device(&self) -> &CameraDevice {
        &self.device
    }

    /// Capture-lifetime flag observations, not counts of frames in any file.
    /// The reader retains only identity and atomics, never native resources.
    pub fn metadata_reader(&self) -> Arc<dyn Fn() -> CameraCaptureMetadata + Send + Sync> {
        let flags = self.state.observed.clone();
        let id = self.device.id.clone();
        let name = self.device.name.clone();
        Arc::new(move || {
            let mode = flags.mode.lock().clone();
            let bits = flags.delivery.load(Ordering::Relaxed);
            let paths: Vec<_> = [
                (1, "dxgi-native"),
                (2, "cpu-native-upload"),
                (4, "dxgi-gpu-convert"),
                (8, "cpu-upload-gpu-convert"),
            ]
            .into_iter()
            .filter_map(|(bit, name)| (bits & bit != 0).then_some(name))
            .collect();
            CameraCaptureMetadata {
                device_id: id.clone(),
                device_name: name.clone(),
                media_type_interlace_mode: flags.media_type_interlace_mode.load(Ordering::Relaxed),
                field_handling: "weave".into(),
                delivery: format!(
                    "{}{}:{}",
                    if mode.shared { "frameserver" } else { "mf" },
                    if mode.decoded_format.is_empty() {
                        ""
                    } else {
                        "-mjpeg-mf-decode"
                    },
                    paths.join("+")
                ),
                native_format: mode.native_format,
                decoded_format: mode.decoded_format,
                native_frame_rate: mode.native_frame_rate,
                color_space: mode.color,
                sample_interlaced: observed(&flags.interlaced),
                sample_bottom_field_first: observed(&flags.bottom_first),
                sample_single_field: observed(&flags.single_field),
                sample_repeat_first_field: observed(&flags.repeat_first),
            }
        })
    }

    pub fn stats(&self) -> CameraCaptureStats {
        let counts = &self.state.counts;
        CameraCaptureStats {
            received_frames: counts.received_frames.load(Ordering::Relaxed),
            delivered_frames: counts.delivered_frames.load(Ordering::Relaxed),
            pool_drops: counts.pool_drops.load(Ordering::Relaxed),
            discontinuities: counts.discontinuities.load(Ordering::Relaxed),
            timestamp_errors: counts.timestamp_errors.load(Ordering::Relaxed),
            format_changes: counts.format_changes.load(Ordering::Relaxed),
            gap_events: counts.gap_events.load(Ordering::Relaxed),
            stream_ticks: counts.stream_ticks.load(Ordering::Relaxed),
            woven_frames: counts.woven_frames.load(Ordering::Relaxed),
            bottom_first_frames: counts.bottom_first_frames.load(Ordering::Relaxed),
        }
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        self.state.stop.signal();
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// Enumerate identities and native modes on a private, joined COM thread.
/// Bad devices keep their identity with empty formats and a diagnostic; they
/// cannot prevent listing another camera. No ReadSample is issued here.
pub fn camera_devices() -> Result<Vec<CameraDevice>> {
    thread::Builder::new()
        .name("Windows camera devices".into())
        .spawn(|| {
            let result = (|| -> Result<_> {
                let _apartment = Apartment::new()?;
                let _runtime = Runtime::new()?;
                let mut devices = Vec::new();
                for identity in identities()? {
                    let mut device = identity.device.clone();
                    let formats = (|| -> Result<_> {
                        let source = Source::open(identity.activation)?;
                        let reader =
                            unsafe { MFCreateSourceReaderFromMediaSource(&source.raw, None) }?;
                        Ok(
                            native_formats(&reader, MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?
                                .into_iter()
                                .map(|mode| mode.format)
                                .collect(),
                        )
                    })();
                    match formats {
                        Ok(formats) => device.formats = formats,
                        Err(error) => eprintln!(
                            "Windows camera devices: {:?} ({}): {error:#}",
                            device.name, device.id
                        ),
                    }
                    devices.push(device);
                }
                Ok(devices)
            })();
            result.map_err(|error| format!("Windows camera devices: {error:#}"))
        })?
        .join()
        .map_err(|_| anyhow!("Windows camera enumeration panicked"))?
        .map_err(anyhow::Error::msg)
}

pub fn capture_camera(
    graphics: &GraphicsDevice,
    device: &str,
    request: CameraRequest,
    canvas: &Canvas,
    sink: VideoSink<Frame>,
    thread_start: fn(),
) -> Result<(CameraCapture, CaptureFormat)> {
    if request.fps.is_some_and(|fps| !(1..=240).contains(&fps)) {
        bail!("camera.fps: valid integer range is 1..=240");
    }
    if request
        .size
        .is_some_and(|(width, height)| width == 0 || height == 0 || width > 16384 || height > 16384)
    {
        bail!("camera.size: width and height must be 1..=16384");
    }
    check_quarantine()?;
    let state = Arc::new(State {
        stop: Event::new()?,
        wake: Event::new()?,
        stopping: AtomicBool::new(false),
        mailbox: Mutex::new(Mailbox::default()),
        counts: Counters::default(),
        observed: Arc::new(ObservedFlags::default()),
    });
    let (started, ready) = mpsc::sync_channel(1);
    let query = device.to_owned();
    let canvas = canvas.clone();
    let worker =
        {
            let graphics = graphics.clone();
            let state = state.clone();
            thread::Builder::new()
                .name("Windows camera capture".into())
                .spawn(move || {
                    let mut running = false;
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                            let _apartment = Apartment::new()?;
                            let _runtime = Runtime::new()?;
                            thread_start();
                            let identity = resolve(&query)?;
                            let device = identity.device.clone();
                            let result = (|| {
                                let mut capture = Session::new(
                                    graphics.clone(),
                                    identity,
                                    request,
                                    &canvas,
                                    state.clone(),
                                )?;
                                capture.run(&sink, &started, &mut running)
                            })();
                            // A controlling source cannot start while another app
                            // owns the camera. Release only OUR source and attach
                            // read-only; never change the owner's selected format.
                            if !running
                                && result.as_ref().is_err_and(|error| {
                                    error.downcast_ref::<windows::core::Error>().is_some_and(
                                        |error| error.code() == MF_E_HW_MFT_FAILED_START_STREAMING,
                                    )
                                })
                            {
                                let mut shared =
                                    SharedCamera::new(graphics, device, request, state.clone())
                                        .context("camera shared-mode fallback")?;
                                shared.run(&sink, &started, &mut running)
                            } else {
                                result
                            }
                        }));
                    // Also covers failures after callback registration but before
                    // Session construction. No queued COM reference may be left
                    // for the caller's apartment to release.
                    state.close();
                    let error = match result {
                        Ok(Ok(())) => return,
                        Ok(Err(error)) => format!("Windows camera capture: {error:#}"),
                        Err(_) => "Windows camera capture worker panicked".into(),
                    };
                    if !running {
                        let _ = started.send(Err(error));
                    } else if !state.stopping.load(Ordering::Acquire) {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            (sink.stopped)(error)
                        }));
                    }
                })?
        };
    match ready.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok((device, format))) => Ok((
            CameraCapture {
                state,
                worker: Some(worker),
                device,
            },
            format,
        )),
        result => {
            state.stopping.store(true, Ordering::Release);
            state.stop.signal();
            // Native activation/shutdown cannot be forcibly interrupted. Join
            // even on timeout; never detach an OS-open worker on each retry.
            let _ = worker.join();
            match result {
                Ok(Err(error)) => bail!("{error}"),
                Err(error) => Err(error).context("Windows camera did not start in time"),
                _ => unreachable!(),
            }
        }
    }
}

struct Activation(IMFActivate);

impl Drop for Activation {
    fn drop(&mut self) {
        shutdown_result(unsafe { self.0.ShutdownObject() }, "activation shutdown");
    }
}

struct Source {
    raw: IMFMediaSource,
    _activation: Activation,
}

impl Source {
    fn open(activation: IMFActivate) -> Result<Self> {
        let activation = Activation(activation);
        let raw = unsafe { activation.0.ActivateObject() }.context("MF camera activation")?;
        Ok(Self {
            raw,
            _activation: activation,
        })
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        shutdown_result(unsafe { self.raw.Shutdown() }, "source shutdown");
    }
}

fn shutdown_result(result: windows::core::Result<()>, operation: &str) {
    if let Err(error) = result
        && error.code() != MF_E_SHUTDOWN
    {
        eprintln!("Windows camera {operation}: {error}");
    }
}

struct DeviceArray {
    items: *mut Option<IMFActivate>,
    count: u32,
}

impl Drop for DeviceArray {
    fn drop(&mut self) {
        if !self.items.is_null() {
            // Even a rejected oversized list owns all returned COM references.
            for index in 0..self.count as usize {
                drop(unsafe { (*self.items.add(index)).take() });
            }
            unsafe { CoTaskMemFree(Some(self.items.cast())) };
        }
    }
}

struct Identity {
    activation: IMFActivate,
    device: CameraDevice,
}

fn attributes() -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, 4) }?;
    attributes.context("MF returned no attributes")
}

fn attribute_string(attributes: &IMFAttributes, key: &GUID) -> Result<String> {
    let length = unsafe { attributes.GetStringLength(key) }?;
    if length == 0 || length > 32768 {
        bail!("MF camera identity length {length} is invalid");
    }
    let mut value = vec![0; length as usize + 1];
    let mut actual = 0;
    unsafe { attributes.GetString(key, &mut value, Some(&mut actual)) }?;
    if actual > length || value[..actual as usize].contains(&0) {
        bail!("MF camera identity changed or contains NUL");
    }
    Ok(String::from_utf16(&value[..actual as usize])?)
}

fn identities() -> Result<Vec<Identity>> {
    let attributes = attributes()?;
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }?;
    let mut list = DeviceArray {
        items: std::ptr::null_mut(),
        count: 0,
    };
    unsafe { MFEnumDeviceSources(&attributes, &mut list.items, &mut list.count) }?;
    if list.count > MAX_DEVICES || (list.count != 0 && list.items.is_null()) {
        bail!(
            "MF camera enumeration returned an invalid list ({} devices; limit {MAX_DEVICES})",
            list.count
        );
    }
    let mut devices = Vec::new();
    for index in 0..list.count as usize {
        let identity = (|| -> Result<_> {
            let activation =
                unsafe { (*list.items.add(index)).take() }.context("null camera activation")?;
            let id = attribute_string(
                &activation,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            )?;
            let name = attribute_string(&activation, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)?;
            Ok(Identity {
                activation,
                device: CameraDevice {
                    id,
                    name,
                    formats: Vec::new(),
                },
            })
        })();
        match identity {
            Ok(identity) => devices.push(identity),
            Err(error) => eprintln!("Windows camera devices: skipping identity {index}: {error:#}"),
        }
    }
    Ok(devices)
}

fn resolve(query: &str) -> Result<Identity> {
    let mut devices = identities()?;
    if let Some(index) = devices
        .iter()
        .position(|identity| identity.device.id == query)
    {
        return Ok(devices.remove(index));
    }
    let lower = query.to_lowercase();
    let exact = devices
        .iter()
        .any(|identity| identity.device.name.to_lowercase() == lower);
    let matches: Vec<_> = devices
        .iter()
        .enumerate()
        .filter(|(_, identity)| {
            !query.is_empty()
                && !query.contains('\0')
                && !query.eq_ignore_ascii_case("default")
                && if exact {
                    identity.device.name.to_lowercase() == lower
                } else {
                    identity.device.name.to_lowercase().contains(&lower)
                }
        })
        .map(|(index, _)| index)
        .collect();
    if matches.len() == 1 {
        return Ok(devices.remove(matches[0]));
    }
    let choices = devices
        .iter()
        .map(|identity| format!("{} ({})", identity.device.name, identity.device.id))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "camera.device: {query:?} matched {} cameras; use an exact symbolic link, exact name or unambiguous name substring; valid choices: {}",
        matches.len(),
        if choices.is_empty() {
            "(none)"
        } else {
            &choices
        }
    );
}

struct NativeFormat {
    raw: IMFMediaType,
    format: CameraFormat,
}

fn optional_u32(attributes: &IMFAttributes, key: &GUID) -> Result<Option<u32>> {
    match unsafe { attributes.GetUINT32(key) } {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn qualified_format(media_type: &IMFMediaType) -> Result<CameraFormat> {
    unsafe {
        if media_type.GetGUID(&MF_MT_MAJOR_TYPE)? != MFMediaType_Video {
            bail!("camera.format: expected a video media type");
        }
        let pixel_format = CameraPixelFormat::from_guid(media_type.GetGUID(&MF_MT_SUBTYPE)?)?;
        let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let width = (size >> 32) as u32;
        let height = size as u32;
        let rate = media_type.GetUINT64(&MF_MT_FRAME_RATE)?;
        let numerator = (rate >> 32) as u32;
        let denominator = rate as u32;
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || (matches!(
                pixel_format,
                CameraPixelFormat::Nv12
                    | CameraPixelFormat::Yuy2
                    | CameraPixelFormat::Uyvy
                    | CameraPixelFormat::Mjpg
            ) && !width.is_multiple_of(2))
            || (matches!(
                pixel_format,
                CameraPixelFormat::Nv12 | CameraPixelFormat::Mjpg
            ) && !height.is_multiple_of(2))
            || u64::from(width) * u64::from(height) * 4 > 256 * 1024 * 1024
        {
            bail!("camera.size: invalid/beyond 256MiB {pixel_format:?} size {width}x{height}");
        }
        let fps = nominal_rate(numerator, denominator)?;
        let interlace = optional_u32(media_type, &MF_MT_INTERLACE_MODE)?.unwrap_or(0);
        if ![
            MFVideoInterlace_Unknown.0 as u32,
            MFVideoInterlace_Progressive.0 as u32,
            MFVideoInterlace_FieldInterleavedUpperFirst.0 as u32,
            MFVideoInterlace_FieldInterleavedLowerFirst.0 as u32,
            MFVideoInterlace_MixedInterlaceOrProgressive.0 as u32,
        ]
        .contains(&interlace)
        {
            bail!(
                "camera.format: interlace mode {interlace}; valid modes are progressive or two interleaved fields"
            );
        }
        // Missing colour metadata is common on virtual cameras. SDR defaults
        // are explicit below, rather than treating an absent attribute as an
        // unsupported pixel format. HDR still needs a separate tone-map path.
        if let Some(transfer) = optional_u32(media_type, &MF_MT_TRANSFER_FUNCTION)?
            && ![
                0,
                MFVideoTransFunc_709.0 as u32,
                MFVideoTransFunc_sRGB.0 as u32,
            ]
            .contains(&transfer)
        {
            bail!(
                "camera.format: transfer function {transfer}; supported: SDR BT.709, sRGB or unspecified"
            );
        }
        Ok(CameraFormat {
            width,
            height,
            fps,
            media_type_interlace_mode: interlace,
            pixel_format,
            rate_numerator: numerator,
            rate_denominator: denominator,
        })
    }
}

fn nominal_rate(numerator: u32, denominator: u32) -> Result<u32> {
    if denominator == 0 {
        bail!("camera.fps: zero denominator")
    }
    let fps = ((u64::from(numerator) + u64::from(denominator) / 2) / u64::from(denominator)) as u32;
    let nominal = u64::from(fps) * u64::from(denominator);
    // Keep the actual ratio in metadata. The integer setup clock accepts the
    // usual 1000/1001 rates and UVC interval rounding, not arbitrary rounding.
    if !(1..=240).contains(&fps) || u64::from(numerator).abs_diff(nominal) * 10_000 > nominal * 11 {
        bail!(
            "camera.fps: {numerator}/{denominator}; supported nominal rates1..=240 (within0.11%)"
        );
    }
    Ok(fps)
}

fn input_layout(media: &IMFMediaType, format: CameraFormat) -> Result<Layout> {
    let mut stride = optional_u32(media, &MF_MT_DEFAULT_STRIDE)?.map(|v| v as i32);
    if stride.is_none() && format.pixel_format != CameraPixelFormat::Mjpg {
        let computed = unsafe {
            MFGetStrideForBitmapInfoHeader(media.GetGUID(&MF_MT_SUBTYPE)?.data1, format.width)
        }?;
        stride = Some(computed);
    }
    let matrix = match optional_u32(media, &MF_MT_YUV_MATRIX)?.unwrap_or(0) {
        0 => {
            if format.height >= 720 {
                709
            } else {
                601
            }
        }
        value if value == MFVideoTransferMatrix_BT709.0 as u32 => 709,
        value if value == MFVideoTransferMatrix_BT601.0 as u32 => 601,
        other => bail!("camera.color.matrix: {other}; supported: BT.601, BT.709 or unspecified"),
    };
    let full_range = match optional_u32(media, &MF_MT_VIDEO_NOMINAL_RANGE)?.unwrap_or(0) {
        0 => matches!(
            format.pixel_format,
            CameraPixelFormat::Rgb24
                | CameraPixelFormat::Rgb32
                | CameraPixelFormat::Argb32
                | CameraPixelFormat::Mjpg
        ),
        value if value == MFNominalRange_0_255.0 as u32 => true,
        value if value == MFNominalRange_16_235.0 as u32 => false,
        other => bail!("camera.color.range: {other}; supported: full, limited or unspecified"),
    };
    Ok(Layout {
        format: format.pixel_format,
        width: format.width,
        height: format.height,
        stride: stride.unwrap_or(0),
        color: Color { matrix, full_range },
    })
}

fn native_formats(reader: &IMFSourceReader, stream: u32) -> Result<Vec<NativeFormat>> {
    let mut formats = Vec::new();
    let mut rejected = 0;
    let mut last_rejection = String::new();
    for index in 0..MAX_FORMATS {
        let raw = match unsafe { reader.GetNativeMediaType(stream, index) } {
            Ok(raw) => raw,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => {
                if formats.is_empty() {
                    bail!(
                        "camera.format: no qualified native modes ({rejected} rejected): {last_rejection}"
                    );
                }
                return Ok(formats);
            }
            Err(error) => return Err(error).context("enumerate native MF camera modes"),
        };
        match qualified_format(&raw).and_then(|format| {
            if format.pixel_format != CameraPixelFormat::Mjpg {
                input_layout(&raw, format)?;
            }
            Ok(format)
        }) {
            Ok(format) => {
                formats.push(NativeFormat { raw, format });
            }
            Err(error) => {
                rejected += 1;
                last_rejection = format!("{error:#}");
            }
        }
    }
    bail!("camera.format: native enumeration did not end within {MAX_FORMATS} entries")
}

fn select_format(
    formats: &[NativeFormat],
    request: CameraRequest,
    canvas: &Canvas,
) -> Result<usize> {
    let wanted = request.size.unwrap_or((canvas.width, canvas.height));
    let fps = request.fps.unwrap_or(canvas.fps);
    // Exact requested fields are constraints. Unspecified fields prefer the
    // nearest canvas dimensions, then nominal rate and conversion cost.
    // 30000/1001 and 30/1 are both nominal30: prefer NV12 before breaking
    // their tie by the exact rate. Stable native order breaks remaining ties.
    let selected = formats
        .iter()
        .enumerate()
        .filter(|(_, mode)| {
            request
                .size
                .is_none_or(|size| size == (mode.format.width, mode.format.height))
                && request.fps.is_none_or(|fps| fps == mode.format.fps)
        })
        .min_by_key(|(_, mode)| {
            (
                u64::from(mode.format.width.abs_diff(wanted.0))
                    + u64::from(mode.format.height.abs_diff(wanted.1)),
                mode.format.fps.abs_diff(fps),
                mode.format.pixel_format.rank(),
                u64::from(mode.format.rate_numerator)
                    .abs_diff(u64::from(fps) * u64::from(mode.format.rate_denominator))
                    * 1_000_000
                    / u64::from(mode.format.rate_denominator),
                input_layout(&mode.raw, mode.format).map_or(0, |layout| {
                    u8::from(
                        layout.format == CameraPixelFormat::Nv12
                            && (layout.color.matrix != 709 || layout.color.full_range),
                    )
                }),
            )
        });
    if let Some((index, _)) = selected {
        return Ok(index);
    }
    let choices = formats
        .iter()
        .map(|mode| {
            format!(
                "{}x{}@{} {}",
                mode.format.width,
                mode.format.height,
                mode.format.fps,
                mode.format.pixel_format.name()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "camera.size/fps: request {request:?} has no exact native match; qualified choices: {choices}"
    )
}

fn video_stream(source: &IMFMediaSource) -> Result<u32> {
    let presentation = unsafe { source.CreatePresentationDescriptor() }?;
    let count = unsafe { presentation.GetStreamDescriptorCount() }?;
    if count > MAX_DEVICES {
        bail!("MF camera exposes too many streams: {count}");
    }
    for index in 0..count {
        let mut selected = Default::default();
        let mut descriptor = None;
        unsafe { presentation.GetStreamDescriptorByIndex(index, &mut selected, &mut descriptor) }?;
        let descriptor = descriptor.context("MF camera returned no stream descriptor")?;
        if unsafe { descriptor.GetMediaTypeHandler()?.GetMajorType() }? == MFMediaType_Video {
            return Ok(index);
        }
    }
    bail!("MF camera exposes no video stream")
}

struct Pending {
    frame: Frame,
    _sample: IMFSample,
    submitted: Instant,
}

struct Resources {
    // Reader releases before source shutdown on a normal drop. The manager,
    // source and texture pool remain together if a GPU copy is quarantined.
    reader: IMFSourceReader,
    _source: Source,
    _manager: IMFDXGIDeviceManager,
    pixels: CameraPixels,
    decoder: Option<JpegDecoder>,
    graphics: GraphicsDevice,
    pending: VecDeque<Pending>,
    uncertain: Option<IMFSample>,
}

impl Resources {
    fn reap(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let completed = self.graphics.completed()?;
        while self
            .pending
            .front()
            .is_some_and(|pending| pending.frame.ready_value() <= completed)
        {
            self.pending.pop_front();
        }
        Ok(())
    }
}

struct Session {
    resources: Option<Resources>,
    state: Arc<State>,
    device: CameraDevice,
    format: CameraFormat,
    stream: u32,
    previous: Option<Time>,
}

impl Session {
    fn new(
        graphics: GraphicsDevice,
        identity: Identity,
        request: CameraRequest,
        canvas: &Canvas,
        state: Arc<State>,
    ) -> Result<Self> {
        check_quarantine()?;
        // Resolution has finished before this sole device is activated.
        let source = Source::open(identity.activation).with_context(|| {
            format!(
                "camera.device: {:?} ({})",
                identity.device.name, identity.device.id
            )
        })?;
        let stream = video_stream(&source.raw)?;
        let mut token = 0;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }?;
        let manager = manager.context("MF returned no camera DXGI manager")?;
        unsafe { manager.ResetDevice(graphics.raw(), token) }?;
        let callback: IMFSourceReaderCallback = Callback {
            state: state.clone(),
        }
        .into();
        let attributes = attributes()?;
        unsafe {
            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
            attributes.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)?;
            attributes.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 0)?;
        }
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source.raw, &attributes) }
            .context("create GPU MF camera Source Reader")?;
        unsafe {
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
        }
        let formats = native_formats(&reader, stream)?;
        let selected = &formats[select_format(&formats, request, canvas)?];
        unsafe { reader.SetCurrentMediaType(stream, None, &selected.raw) }
            .context("set exact native MF camera mode")?;
        let current_type = unsafe { reader.GetCurrentMediaType(stream) }?;
        let current = qualified_format(&current_type)?;
        if (
            current.width,
            current.height,
            current.fps,
            current.pixel_format,
        ) != (
            selected.format.width,
            selected.format.height,
            selected.format.fps,
            selected.format.pixel_format,
        ) {
            bail!(
                "camera.format: requested {:?}, driver selected {current:?}",
                selected.format
            );
        }
        let mut device = identity.device;
        device.formats = formats.into_iter().map(|mode| mode.format).collect();
        state
            .observed
            .media_type_interlace_mode
            .store(current.media_type_interlace_mode, Ordering::Relaxed);
        let decoder = (current.pixel_format == CameraPixelFormat::Mjpg)
            .then(|| JpegDecoder::new(current.width, current.height, current.fps))
            .transpose()?;
        let layout = match &decoder {
            Some(decoder) => decoder.layout(),
            None => input_layout(&current_type, current)?,
        };
        describe_mode(&state, current, layout, false);
        Ok(Self {
            resources: Some(Resources {
                reader,
                _source: source,
                _manager: manager,
                pixels: CameraPixels::new(graphics.clone(), layout)?,
                decoder,
                graphics,
                pending: VecDeque::with_capacity(IN_FLIGHT),
                uncertain: None,
            }),
            state,
            device,
            format: current,
            stream,
            previous: None,
        })
    }

    fn run(
        &mut self,
        sink: &VideoSink<Frame>,
        started: &mpsc::SyncSender<Startup>,
        running: &mut bool,
    ) -> Result<()> {
        let deadline = Instant::now() + START_WAIT;
        let mut outstanding = false;
        loop {
            if self.state.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            check_quarantine()?;
            if !*running && Instant::now() >= deadline {
                bail!("MF camera delivered no supported frame within 5 seconds");
            }
            if let Some(message) = self.state.next()? {
                match message {
                    Message::Event(event) => {
                        unsafe { event.GetStatus()?.ok() }.context("MF camera source event")?;
                        if unsafe { event.GetType() }? == MEError.0 as u32 {
                            bail!("MF camera source reported MEError");
                        }
                    }
                    Message::Sample {
                        status,
                        stream,
                        flags,
                        sample,
                        arrived,
                    } => {
                        status.ok().context("MF camera ReadSample callback")?;
                        if !outstanding || stream != self.stream {
                            bail!(
                                "MF camera returned an unsolicited sample or wrong stream {stream}"
                            );
                        }
                        outstanding = false;
                        self.check_flags(flags, *running)?;
                        if let Some(sample) = sample {
                            let at = self.timestamp(&sample, arrived).inspect_err(|_| {
                                self.state
                                    .counts
                                    .timestamp_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            })?;
                            if let Some(frame) = self.copy(sample)? {
                                if !*running {
                                    let format = CaptureFormat {
                                        width: self.format.width,
                                        height: self.format.height,
                                        fps: self.format.fps,
                                    };
                                    if started.send(Ok((self.device.clone(), format))).is_err() {
                                        return Ok(());
                                    }
                                    *running = true;
                                }
                                if self.state.stopping.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                (sink.frame)(frame, at);
                                self.state
                                    .counts
                                    .delivered_frames
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                continue;
            }
            let resources = self.resources.as_mut().expect("open camera");
            resources.reap()?;
            if resources
                .pending
                .front()
                .is_some_and(|pending| pending.submitted.elapsed() >= COPY_WAIT)
            {
                bail!("MF camera GPU copy did not complete within 5 seconds");
            }
            if !outstanding && resources.pending.len() < IN_FLIGHT {
                // One request at a time; no ReadSample is issued by callbacks.
                outstanding = true;
                unsafe {
                    resources
                        .reader
                        .ReadSample(self.stream, 0, None, None, None, None)
                }
                .context("MF camera ReadSample")?;
            }
            // A full copy ring is the only steady-state CPU pressure wait.
            // Otherwise the callback event wakes the worker immediately.
            self.state.wait(if resources.pending.len() == IN_FLIGHT {
                10
            } else {
                100
            })?;
        }
    }

    fn check_flags(&mut self, flags: u32, running: bool) -> Result<()> {
        if flags & MF_SOURCE_READERF_STREAMTICK.0 as u32 != 0 {
            self.state
                .counts
                .stream_ticks
                .fetch_add(1, Ordering::Relaxed);
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            bail!("MF camera Source Reader error flag");
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            bail!("MF camera source ended");
        }
        let changes = MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32
            | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32
            | MF_SOURCE_READERF_NEWSTREAM.0 as u32
            | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32;
        if flags & changes != 0 {
            self.state
                .counts
                .format_changes
                .fetch_add(1, Ordering::Relaxed);
            let reader = &self.resources.as_ref().expect("open camera").reader;
            let media_type = unsafe { reader.GetCurrentMediaType(self.stream) }?;
            let current = qualified_format(&media_type)?;
            if running
                || (
                    current.width,
                    current.height,
                    current.fps,
                    current.pixel_format,
                ) != (
                    self.format.width,
                    self.format.height,
                    self.format.fps,
                    self.format.pixel_format,
                )
                || flags
                    & (MF_SOURCE_READERF_NEWSTREAM.0 as u32
                        | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32)
                    != 0
            {
                bail!(
                    "camera.format changed (flags {flags:#x}, current {current:?}); reopen explicitly"
                );
            }
            // Before the first picture a source may fill in stride/colour/
            // interlace metadata. Use that actual layout, not stale defaults.
            let resources = self.resources.as_mut().expect("open camera");
            let layout = match &resources.decoder {
                Some(decoder) => decoder.layout(),
                None => input_layout(&media_type, current)?,
            };
            resources.pixels = CameraPixels::new(resources.graphics.clone(), layout)?;
            self.format = current;
            self.state
                .observed
                .media_type_interlace_mode
                .store(current.media_type_interlace_mode, Ordering::Relaxed);
            describe_mode(&self.state, current, layout, false);
        }
        Ok(())
    }

    fn timestamp(&mut self, sample: &IMFSample, arrived: Time) -> Result<Time> {
        sample_time(
            &self.state,
            &mut self.previous,
            self.format.fps,
            sample,
            arrived,
        )
    }

    fn copy(&mut self, sample: IMFSample) -> Result<Option<Frame>> {
        let (interlaced, bottom_first) = sample_fields(&self.state, self.format, &sample)?;
        let resources = self.resources.as_mut().expect("open camera");
        let decoded = resources
            .decoder
            .as_mut()
            .map(|decoder| decoder.decode(&sample))
            .transpose()?;
        // Retain the original GPU sample before any possible GPU submission.
        // A software decoder's CPU output is copied by UpdateSubresource
        // before returning, so it is not an asynchronous input lease.
        resources.uncertain = Some(sample.clone());
        let converted = resources.pixels.sample(decoded.as_ref().unwrap_or(&sample));
        if converted.is_err() && !resources.pixels.uncertain() {
            resources.uncertain = None;
        }
        let converted = converted?;
        let sample = resources.uncertain.take().expect("copy source retained");
        let frame = if let Some((frame, delivery)) = converted {
            note_delivery(&self.state, delivery);
            count_fields(&self.state, interlaced, bottom_first);
            resources.pending.push_back(Pending {
                frame: frame.clone(),
                _sample: sample,
                submitted: Instant::now(),
            });
            Some(frame)
        } else {
            self.state.counts.pool_drops.fetch_add(1, Ordering::Relaxed);
            None
        };
        Ok(frame)
    }
}

fn sample_time(
    state: &State,
    previous: &mut Option<Time>,
    fps: u32,
    sample: &IMFAttributes,
    arrived: Time,
) -> Result<Time> {
    if optional_u32(sample, &MFSampleExtension_Discontinuity)?.unwrap_or(0) != 0 {
        state.counts.discontinuities.fetch_add(1, Ordering::Relaxed);
    }
    // DeviceTimestamp is absolute QPC in 100 ns units. SampleTime may be
    // stream-relative; arrival is only a future-timestamp sanity check.
    let ticks = unsafe { sample.GetUINT64(&MFSampleExtension_DeviceTimestamp) }
        .context("camera.timestamp: missing QPC device timestamp; no arrival-time fallback")?;
    let nanos = ticks
        .checked_mul(100)
        .and_then(|nanos| i64::try_from(nanos).ok())
        .context("camera.timestamp: QPC100ns overflow")?;
    let at = Time::from_nanos(nanos);
    if ticks == 0
        || previous.is_some_and(|previous| at <= previous)
        || nanos > arrived.as_nanos().saturating_add(100_000_000)
    {
        bail!(
            "camera.timestamp: zero, non-advancing or future QPC device timestamp {ticks} (arrival {}ns)",
            arrived.as_nanos()
        );
    }
    if previous.is_some_and(|previous| nanos - previous.as_nanos() > 1_500_000_000 / i64::from(fps))
    {
        state.counts.gap_events.fetch_add(1, Ordering::Relaxed);
    }
    *previous = Some(at);
    Ok(at)
}

fn sample_fields(
    state: &State,
    format: CameraFormat,
    sample: &IMFAttributes,
) -> Result<(bool, bool)> {
    let interlaced = optional_u32(sample, &MFSampleExtension_Interlaced)?.map(|value| value != 0);
    let bottom_first =
        optional_u32(sample, &MFSampleExtension_BottomFieldFirst)?.map(|value| value != 0);
    let single_field =
        optional_u32(sample, &MFSampleExtension_SingleField)?.map(|value| value != 0);
    let repeat_first =
        optional_u32(sample, &MFSampleExtension_RepeatFirstField)?.map(|value| value != 0);
    let flags = &state.observed;
    observe(&flags.interlaced, interlaced);
    observe(&flags.bottom_first, bottom_first);
    observe(&flags.single_field, single_field);
    observe(&flags.repeat_first, repeat_first);
    let interlaced = interlaced.unwrap_or(
        format.media_type_interlace_mode == MFVideoInterlace_FieldInterleavedUpperFirst.0 as u32
            || format.media_type_interlace_mode
                == MFVideoInterlace_FieldInterleavedLowerFirst.0 as u32,
    );
    let bottom_first = bottom_first.unwrap_or(
        format.media_type_interlace_mode == MFVideoInterlace_FieldInterleavedLowerFirst.0 as u32,
    );
    // Sample flags override the media type. MF's two-field NV12 buffer
    // already interleaves top-field even rows and bottom-field odd rows.
    // A GPU copy of both planes is a lossless weave, including when a
    // progressive source is mislabelled. BottomFieldFirst changes time
    // order, not row parity: never swap or duplicate its scan lines.
    if single_field == Some(true) {
        bail!(
            "camera.format: single-field samples require field pairing; expected two interleaved fields"
        );
    }
    Ok((interlaced, bottom_first))
}

fn count_fields(state: &State, interlaced: bool, bottom_first: bool) {
    if interlaced {
        state.counts.woven_frames.fetch_add(1, Ordering::Relaxed);
        if bottom_first {
            state
                .counts
                .bottom_first_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.state.close();
        let Some(mut resources) = self.resources.take() else {
            return;
        };
        shutdown_result(
            unsafe {
                resources
                    .reader
                    .Flush(MF_SOURCE_READER_ALL_STREAMS.0 as u32)
            },
            "reader flush",
        );
        let deadline = Instant::now() + COPY_WAIT;
        let drained = (|| -> Result<()> {
            if resources.uncertain.is_some() {
                bail!("copy submission failed without a trustworthy completion fence");
            }
            loop {
                resources.reap()?;
                if resources.pending.is_empty() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("GPU copies did not finish within 5 seconds");
                }
                // Teardown only: no event registration can outlive this wait.
                thread::sleep(Duration::from_millis(1));
            }
        })();
        if let Err(error) = drained {
            QUARANTINED.store(true, Ordering::Release);
            // Flush cancels the reader request; Stop additionally stops the
            // producer. Do not destroy it or its outstanding sample storage
            // while a queued GPU copy might still be reading that storage.
            shutdown_result(
                unsafe { resources._source.raw.Stop() },
                "quarantined source stop",
            );
            eprintln!(
                "Windows camera teardown: {error:#}; retaining source, samples, frames and device until process exit; camera capture disabled"
            );
            // No detached cleanup thread and no use-after-recycle. This is a
            // fatal GPU quarantine, not a reusable stop/reopen path.
            std::mem::forget(resources);
        }
    }
}

fn check_quarantine() -> Result<()> {
    if QUARANTINED.load(Ordering::Acquire) {
        bail!(
            "camera GPU resources were quarantined after a failed copy; restart the process before reopening capture"
        );
    }
    Ok(())
}

// Windows 10 exposes shared capture through MediaCapture, not the newer
// MF_DEVSOURCE_ATTRIBUTE_FRAMESERVER_SHARE_MODE (Windows 11 26100). Keep the
// controlling application's mode. Compressed buffers and CPU bitmaps share
// the Source Reader's conversion path; GPU-only Frame Server surfaces are
// rejected rather than silently downloaded.
struct SharedCamera {
    capture: MediaCapture,
    reader: Option<MediaFrameReader>,
    on_frame: Option<i64>,
    on_error: Option<i64>,
    error: Arc<Mutex<Option<String>>>,
    state: Arc<State>,
    device: CameraDevice,
    format: Option<CameraFormat>,
    graphics: GraphicsDevice,
    pixels: Option<CameraPixels>,
    decoder: Option<JpegDecoder>,
    layout: Option<Layout>,
    pending: VecDeque<(Frame, Instant)>,
    staging: Vec<u8>,
    previous: Option<Time>,
}

impl SharedCamera {
    fn new(
        graphics: GraphicsDevice,
        device: CameraDevice,
        request: CameraRequest,
        state: Arc<State>,
    ) -> Result<Self> {
        check_quarantine()?;
        let group = MediaFrameSourceGroup::FindAllAsync()?
            .join()?
            .into_iter()
            .find(|group| {
                group
                    .Id()
                    .is_ok_and(|id| id.to_string_lossy().eq_ignore_ascii_case(&device.id))
            })
            .context("camera.device: no Frame Server group for the resolved symbolic link")?;
        let settings = MediaCaptureInitializationSettings::new()?;
        settings.SetSourceGroup(&group)?;
        settings.SetSharingMode(MediaCaptureSharingMode::SharedReadOnly)?;
        settings.SetStreamingCaptureMode(StreamingCaptureMode::Video)?;
        settings.SetMemoryPreference(MediaCaptureMemoryPreference::Auto)?;
        let mut session = Self {
            capture: MediaCapture::new()?,
            reader: None,
            on_frame: None,
            on_error: None,
            error: Arc::new(Mutex::new(None)),
            state,
            device,
            format: None,
            graphics: graphics.clone(),
            pixels: None,
            decoder: None,
            layout: None,
            pending: VecDeque::with_capacity(IN_FLIGHT),
            staging: Vec::new(),
            previous: None,
        };
        let error = session.error.clone();
        let state = session.state.clone();
        session.on_error = Some(
            session
                .capture
                .Failed(&MediaCaptureFailedEventHandler::new(move |_, args| {
                    let message = match args.as_ref() {
                        Some(args) => format!(
                            "shared camera failed ({:#x}): {}",
                            args.Code()?,
                            args.Message()?
                        ),
                        None => "shared camera failed without error details".into(),
                    };
                    error.lock().get_or_insert(message);
                    state.wake.signal();
                    Ok(())
                }))?,
        );
        session
            .capture
            .InitializeWithSettingsAsync(&settings)?
            .join()?;
        let source = session
            .capture
            .FrameSources()?
            .into_iter()
            .filter_map(|pair| pair.Value().ok())
            .find(|source| {
                source
                    .Info()
                    .and_then(|info| info.SourceKind())
                    .is_ok_and(|kind| kind == MediaFrameSourceKind::Color)
            })
            .context("camera.device: shared group has no color source")?;
        // Never call SetFormatAsync: the controlling application's choice wins.
        let media_type = shared_media_type(&source.CurrentFormat()?)?;
        let format = qualified_format(&media_type)?;
        if request
            .size
            .is_some_and(|size| size != (format.width, format.height))
            || request.fps.is_some_and(|fps| fps != format.fps)
        {
            bail!(
                "camera.size/fps: shared camera is {}x{}@{}; cannot change the controlling application's format for {request:?}",
                format.width,
                format.height,
                format.fps
            );
        }
        session.decoder = (format.pixel_format == CameraPixelFormat::Mjpg)
            .then(|| JpegDecoder::new(format.width, format.height, format.fps))
            .transpose()?;
        let layout = match &session.decoder {
            Some(decoder) => decoder.layout(),
            None => input_layout(&media_type, format)?,
        };
        session.pixels = Some(CameraPixels::new(graphics, layout)?);
        session.layout = Some(layout);
        session.format = Some(format);
        session
            .state
            .observed
            .media_type_interlace_mode
            .store(format.media_type_interlace_mode, Ordering::Relaxed);
        describe_mode(&session.state, format, layout, true);
        let reader = session.capture.CreateFrameReaderAsync(&source)?.join()?;
        session.reader = Some(reader.clone());
        let state = session.state.clone();
        session.on_frame = Some(reader.FrameArrived(&TypedEventHandler::new(move |_, _| {
            state.wake.signal();
            Ok(())
        }))?);
        let status = reader.StartAsync()?.join()?;
        if status != MediaFrameReaderStartStatus::Success {
            bail!("camera.format: shared reader failed to start: {status:?}");
        }
        Ok(session)
    }

    fn run(
        &mut self,
        sink: &VideoSink<Frame>,
        started: &mpsc::SyncSender<Startup>,
        running: &mut bool,
    ) -> Result<()> {
        let mut last = Instant::now();
        while !self.state.stopping.load(Ordering::Acquire) {
            if let Some(error) = self.error.lock().take() {
                bail!("{error}");
            }
            // A disconnected or stalled producer cannot silently freeze the
            // source forever, even if its driver omits the Failed event.
            if last.elapsed() >= START_WAIT {
                bail!("shared camera delivered no frame for 5 seconds; reopen the source");
            }
            let completed = self.graphics.completed()?;
            while self
                .pending
                .front()
                .is_some_and(|(frame, _)| frame.ready_value() <= completed)
            {
                self.pending.pop_front();
            }
            if self.pending.len() == IN_FLIGHT {
                self.state.wait(10)?;
                continue;
            }
            let reference = match self
                .reader
                .as_ref()
                .expect("shared reader")
                .TryAcquireLatestFrame()
            {
                Ok(reference) => reference,
                // windows-rs represents a successful null WinRT result this
                // way. E_POINTER and other genuine failures are not no-frame.
                Err(error) if error.code() == S_OK => {
                    self.state.wait(100)?;
                    continue;
                }
                Err(error) => return Err(error).context("shared camera acquire"),
            };
            let frame = self.decode(&reference);
            // Decoding and the synchronous CPU upload are finished; nothing on the
            // GPU refers to the compressed source buffer after this point.
            let closed = reference.Close();
            let Some((frame, at)) = frame? else {
                closed?;
                last = Instant::now();
                continue;
            };
            closed?;
            last = Instant::now();
            if !*running {
                let format = self.format.expect("shared format");
                started.send(Ok((
                    self.device.clone(),
                    CaptureFormat {
                        width: format.width,
                        height: format.height,
                        fps: format.fps,
                    },
                )))?;
                *running = true;
            }
            if self.state.stopping.load(Ordering::Acquire) {
                break;
            }
            (sink.frame)(frame, at);
            self.state
                .counts
                .delivered_frames
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn decode(&mut self, reference: &MediaFrameReference) -> Result<Option<(Frame, Time)>> {
        self.state
            .counts
            .received_frames
            .fetch_add(1, Ordering::Relaxed);
        let format = self.format.expect("shared format");
        if qualified_format(&shared_media_type(&reference.Format()?)?)? != format {
            self.state
                .counts
                .format_changes
                .fetch_add(1, Ordering::Relaxed);
            bail!("camera.format changed in shared mode; reopen the source");
        }
        let sample = attributes()?;
        let properties = reference.Properties()?;
        for key in [
            MFSampleExtension_Interlaced,
            MFSampleExtension_BottomFieldFirst,
            MFSampleExtension_SingleField,
            MFSampleExtension_RepeatFirstField,
            MFSampleExtension_Discontinuity,
        ] {
            if properties.HasKey(key)? {
                let value: IPropertyValue = properties.Lookup(key)?.cast()?;
                unsafe { sample.SetUINT32(&key, value.GetUInt32()?) }?;
            }
        }
        // Both properties are OS capture timestamps in QPC 100 ns units,
        // not worker arrival times. Keep the driver's original when present.
        let ticks = if properties.HasKey(MFSampleExtension_DeviceTimestamp)? {
            properties
                .Lookup(MFSampleExtension_DeviceTimestamp)?
                .cast::<IPropertyValue>()?
                .GetUInt64()?
        } else {
            u64::try_from(reference.SystemRelativeTime()?.Value()?.Duration)?
        };
        unsafe { sample.SetUINT64(&MFSampleExtension_DeviceTimestamp, ticks) }?;
        let at = sample_time(
            &self.state,
            &mut self.previous,
            format.fps,
            &sample,
            clock::now(),
        )
        .inspect_err(|_| {
            self.state
                .counts
                .timestamp_errors
                .fetch_add(1, Ordering::Relaxed);
        })?;
        let (interlaced, bottom_first) = sample_fields(&self.state, format, &sample)?;
        let converted = match reference.BufferMediaFrame() {
            Ok(raw) => {
                let buffer = raw.Buffer()?;
                let length = buffer.Length()? as usize;
                let limit = if self.decoder.is_some() { 64 } else { 256 } * 1024 * 1024;
                if length == 0 || length > limit {
                    bail!("camera.frame: invalid shared buffer length {length}")
                }
                let access: IBufferByteAccess = buffer.cast()?;
                let pointer = unsafe { access.Buffer() }?;
                if pointer.is_null() {
                    bail!("camera.frame: null shared buffer")
                }
                let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
                if let Some(decoder) = &mut self.decoder {
                    let compressed = compressed_sample(bytes, ticks)?;
                    let decoded = decoder.decode(&compressed)?;
                    self.pixels
                        .as_mut()
                        .expect("shared pixels")
                        .sample(&decoded)?
                } else {
                    self.pixels
                        .as_mut()
                        .expect("shared pixels")
                        .bytes(bytes, self.layout.expect("layout").stride)?
                }
            }
            Err(error) if error.code() == S_OK => self.bitmap(reference)?,
            Err(error) => return Err(error).context("camera.frame: shared buffer"),
        };
        let frame = if let Some((frame, delivery)) = converted {
            note_delivery(&self.state, delivery);
            count_fields(&self.state, interlaced, bottom_first);
            self.pending.push_back((frame.clone(), Instant::now()));
            Some(frame)
        } else {
            self.state.counts.pool_drops.fetch_add(1, Ordering::Relaxed);
            None
        };
        Ok(frame.map(|frame| (frame, at)))
    }

    fn bitmap(&mut self, reference: &MediaFrameReference) -> Result<Option<(Frame, Delivery)>> {
        let layout = self.layout.expect("shared layout");
        let bitmap = reference.VideoMediaFrame()?.SoftwareBitmap().context(
            "camera.frame: shared reader supplied no CPU bitmap; no GPU readback fallback",
        )?;
        let _bitmap = CloseOnDrop(bitmap.cast()?);
        let actual = bitmap.BitmapPixelFormat()?;
        let matches = match layout.format {
            CameraPixelFormat::Nv12 => actual == BitmapPixelFormat::Nv12,
            CameraPixelFormat::Yuy2 => actual == BitmapPixelFormat::Yuy2,
            CameraPixelFormat::Rgb32 | CameraPixelFormat::Argb32 => {
                actual == BitmapPixelFormat::Bgra8
            }
            _ => false,
        };
        if !matches
            || bitmap.PixelWidth()? != layout.width as i32
            || bitmap.PixelHeight()? != layout.height as i32
        {
            bail!("camera.frame: shared bitmap {actual:?} does not match negotiated {layout:?}");
        }
        let locked = bitmap.LockBuffer(BitmapBufferAccessMode::Read)?;
        let _locked = CloseOnDrop(locked.cast()?);
        let memory = locked.CreateReference()?;
        let _memory = CloseOnDrop(memory.cast()?);
        let access: IMemoryBufferByteAccess = memory.cast()?;
        let (mut pointer, mut length) = (std::ptr::null_mut(), 0);
        unsafe { access.GetBuffer(&mut pointer, &mut length) }?;
        if pointer.is_null() || length == 0 {
            bail!("camera.frame: empty bitmap memory")
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer, length as usize) };
        let row_bytes = layout.format.row_bytes(layout.width)? as usize;
        let rows = layout.height as usize
            * if layout.format == CameraPixelFormat::Nv12 {
                3
            } else {
                2
            }
            / 2;
        let size = row_bytes
            .checked_mul(rows)
            .filter(|n| *n <= 256 * 1024 * 1024)
            .context("camera.frame: bitmap exceeds256MiB")?;
        self.staging.clear();
        self.staging.try_reserve_exact(size)?;
        let planes = if layout.format == CameraPixelFormat::Nv12 {
            2
        } else {
            1
        };
        if locked.GetPlaneCount()? != planes {
            bail!("camera.frame: unexpected bitmap plane count")
        }
        for plane in 0..planes {
            let desc = locked.GetPlaneDescription(plane)?;
            let rows = if plane == 0 {
                layout.height
            } else {
                layout.height / 2
            };
            if desc.Height < rows as i32 || desc.Stride.unsigned_abs() < row_bytes as u32 {
                bail!("camera.frame: invalid bitmap plane dimensions/stride {desc:?}");
            }
            for y in 0..rows {
                let start = i64::from(desc.StartIndex) + i64::from(y) * i64::from(desc.Stride);
                let start =
                    usize::try_from(start).context("camera.frame: negative bitmap row offset")?;
                let end = start
                    .checked_add(row_bytes)
                    .context("camera.frame: bitmap row overflow")?;
                self.staging.extend_from_slice(
                    bytes
                        .get(start..end)
                        .context("camera.frame: bitmap row exceeds buffer")?,
                );
            }
        }
        // Row packing only; format expansion/colour conversion remains on GPU.
        self.pixels
            .as_mut()
            .expect("shared pixels")
            .bytes(&self.staging, row_bytes as i32)
    }
}

struct CloseOnDrop(IClosable);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}

fn compressed_sample(bytes: &[u8], ticks: u64) -> Result<IMFSample> {
    let sample = unsafe { MFCreateSample() }?;
    let buffer = unsafe { MFCreateMemoryBuffer(bytes.len().try_into()?) }?;
    let mut pointer = std::ptr::null_mut();
    unsafe { buffer.Lock(&mut pointer, None, None) }?;
    if pointer.is_null() {
        let _ = unsafe { buffer.Unlock() };
        bail!("camera.decode: null compressed buffer")
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(ticks.try_into()?)?;
    }
    Ok(sample)
}

impl Drop for SharedCamera {
    fn drop(&mut self) {
        if let Some(reader) = &self.reader {
            if let Some(token) = self.on_frame {
                let _ = reader.RemoveFrameArrived(token);
            }
            if let Ok(stop) = reader.StopAsync() {
                let _ = stop.join();
            }
            let _ = reader.Close();
        }
        if let Some(token) = self.on_error {
            let _ = self.capture.RemoveFailed(token);
        }
        let _ = self.capture.Close();
        let deadline = Instant::now() + COPY_WAIT;
        let drained = (|| -> Result<()> {
            if self.pixels.as_ref().is_some_and(CameraPixels::uncertain) {
                bail!("shared camera GPU write completion is unknown")
            }
            while !self.pending.is_empty() {
                let completed = self.graphics.completed()?;
                while self
                    .pending
                    .front()
                    .is_some_and(|(frame, _)| frame.ready_value() <= completed)
                {
                    self.pending.pop_front();
                }
                if self.pending.is_empty() {
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("shared camera GPU writes did not finish within5s")
                }
                thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        })();
        if let Err(error) = drained {
            QUARANTINED.store(true, Ordering::Release);
            eprintln!(
                "Windows shared camera teardown: {error:#}; retaining owned GPU resources until process exit"
            );
            std::mem::forget(self.pixels.take());
            std::mem::forget(std::mem::take(&mut self.pending));
        }
    }
}

fn shared_media_type(format: &MediaFrameFormat) -> Result<IMFMediaType> {
    let subtype = format.Subtype()?;
    let pixel = CameraPixelFormat::from_name(&subtype.to_string_lossy())?;
    let video = format.VideoFormat()?;
    let rate = format.FrameRate()?;
    let (numerator, denominator) = (rate.Numerator()?, rate.Denominator()?);
    let (width, height) = (video.Width()?, video.Height()?);
    let properties = format.Properties()?;
    let interlace = if properties.HasKey(MF_MT_INTERLACE_MODE)? {
        properties
            .Lookup(MF_MT_INTERLACE_MODE)?
            .cast::<IPropertyValue>()?
            .GetUInt32()?
    } else {
        MFVideoInterlace_Unknown.0 as u32
    };
    let media = unsafe { MFCreateMediaType() }?;
    let guid = match pixel {
        CameraPixelFormat::Nv12 => MFVideoFormat_NV12,
        CameraPixelFormat::Yuy2 => MFVideoFormat_YUY2,
        CameraPixelFormat::Uyvy => MFVideoFormat_UYVY,
        CameraPixelFormat::Rgb24 => MFVideoFormat_RGB24,
        CameraPixelFormat::Rgb32 => MFVideoFormat_RGB32,
        CameraPixelFormat::Argb32 => MFVideoFormat_ARGB32,
        CameraPixelFormat::Mjpg => MFVideoFormat_MJPG,
    };
    unsafe {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, &guid)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        media.SetUINT64(
            &MF_MT_FRAME_RATE,
            (u64::from(numerator) << 32) | u64::from(denominator),
        )?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, interlace)?;
        for key in [
            MF_MT_DEFAULT_STRIDE,
            MF_MT_YUV_MATRIX,
            MF_MT_VIDEO_NOMINAL_RANGE,
            MF_MT_VIDEO_PRIMARIES,
            MF_MT_TRANSFER_FUNCTION,
        ] {
            if properties.HasKey(key)? {
                media.SetUINT32(
                    &key,
                    properties
                        .Lookup(key)?
                        .cast::<IPropertyValue>()?
                        .GetUInt32()?,
                )?;
            }
        }
    }
    Ok(media)
}
