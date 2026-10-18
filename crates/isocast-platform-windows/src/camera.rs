//! Native Media Foundation camera samples, copied GPU-to-GPU into owned NV12
//! frames. Interleaved fields are woven by preserving their existing rows.
//! Busy cameras can supply shared MJPEG through the Windows Frame Server:
//! software JPEG decode and GPU upload, never GPU readback. No interpolation
//! or timestamp rebasing on either path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{CameraRequest, CaptureFormat, FramePool, Time, VideoSink};
use isocast_protocol::{CameraCaptureMetadata, Canvas};
use parking_lot::Mutex;
use windows::Foundation::{IPropertyValue, TypedEventHandler};
use windows::Media::Capture::Frames::{
    MediaFrameFormat, MediaFrameReader, MediaFrameReaderStartStatus, MediaFrameReference,
    MediaFrameSourceGroup, MediaFrameSourceKind,
};
use windows::Media::Capture::{
    MediaCapture, MediaCaptureFailedEventHandler, MediaCaptureInitializationSettings,
    MediaCaptureMemoryPreference, MediaCaptureSharingMode, StreamingCaptureMode,
};
use windows::Win32::Foundation::{S_OK, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device5, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory,
    WICConvertBitmapSource, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::Win32::System::WinRT::IBufferByteAccess;
use windows::core::{GUID, HRESULT, Interface, Ref, implement};

use crate::clock;
use crate::frames::{Frame, GraphicsDevice, TexturePool};
use crate::mf::Runtime;
use crate::native::{Apartment, Event};
use crate::output::OutputPool;

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
}

#[derive(Clone, Debug)]
pub struct CameraDevice {
    /// Exact MF video-capture symbolic link, not the friendly name.
    pub id: String,
    pub name: String,
    /// Qualified native NV12, progressive/two-field, limited-range BT.709 modes.
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
    shared_mjpg: AtomicBool,
    interlaced: AtomicU8,
    bottom_first: AtomicU8,
    single_field: AtomicU8,
    repeat_first: AtomicU8,
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
        Arc::new(move || CameraCaptureMetadata {
            device_id: id.clone(),
            device_name: name.clone(),
            media_type_interlace_mode: flags.media_type_interlace_mode.load(Ordering::Relaxed),
            field_handling: "weave".into(),
            delivery: if flags.shared_mjpg.load(Ordering::Relaxed) {
                "frameserver-mjpg-cpu-decode-upload"
            } else {
                "mf-dxgi-nv12"
            }
            .into(),
            sample_interlaced: observed(&flags.interlaced),
            sample_bottom_field_first: observed(&flags.bottom_first),
            sample_single_field: observed(&flags.single_field),
            sample_repeat_first_field: observed(&flags.repeat_first),
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
    if request.size.is_some_and(|(width, height)| {
        width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2)
    }) {
        bail!("camera.size: NV12 requires positive even width and height");
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
        if media_type.GetGUID(&MF_MT_MAJOR_TYPE)? != MFMediaType_Video
            || media_type.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12
        {
            bail!("camera.format: only native NV12 video is qualified; no decoder or CPU fallback");
        }
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
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            bail!("camera.size: unsupported native NV12 size {width}x{height}");
        }
        if denominator == 0
            || !numerator.is_multiple_of(denominator)
            || !(1..=240).contains(&(numerator / denominator))
        {
            bail!(
                "camera.fps: native rate {numerator}/{denominator}; only exact integers 1..=240 are qualified"
            );
        }
        let interlace = media_type.GetUINT32(&MF_MT_INTERLACE_MODE)?;
        if ![
            MFVideoInterlace_Progressive.0 as u32,
            MFVideoInterlace_FieldInterleavedUpperFirst.0 as u32,
            MFVideoInterlace_FieldInterleavedLowerFirst.0 as u32,
        ]
        .contains(&interlace)
        {
            bail!(
                "camera.format: interlace mode {interlace}; valid modes are progressive or two interleaved fields"
            );
        }
        for (key, expected, label) in [
            (
                &MF_MT_YUV_MATRIX,
                MFVideoTransferMatrix_BT709.0 as u32,
                "BT.709 matrix",
            ),
            (
                &MF_MT_VIDEO_NOMINAL_RANGE,
                MFNominalRange_16_235.0 as u32,
                "16..235 nominal range",
            ),
            (
                &MF_MT_VIDEO_PRIMARIES,
                MFVideoPrimaries_BT709.0 as u32,
                "BT.709 primaries",
            ),
        ] {
            let value = optional_u32(media_type, key)?;
            if value != Some(expected) {
                bail!("camera.format: requires {label} ({expected}), got {value:?}");
            }
        }
        // Transfer metadata can be absent even with explicit BT.709 primaries
        // and matrix. Do not invent an attribute or apply a device-name quirk;
        // reject explicit HDR/non-709 transfer functions.
        if let Some(transfer) = optional_u32(media_type, &MF_MT_TRANSFER_FUNCTION)?
            && transfer != MFVideoTransFunc_709.0 as u32
        {
            bail!(
                "camera.format: transfer function {transfer}; only BT.709 or unspecified is qualified"
            );
        }
        Ok(CameraFormat {
            width,
            height,
            fps: numerator / denominator,
            media_type_interlace_mode: interlace,
        })
    }
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
        match qualified_format(&raw) {
            Ok(format)
                if !formats
                    .iter()
                    .any(|mode: &NativeFormat| mode.format == format) =>
            {
                formats.push(NativeFormat { raw, format });
            }
            Ok(_) => {}
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
    // nearest canvas dimensions, then rate; stable native order breaks ties.
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
            )
        });
    if let Some((index, _)) = selected {
        return Ok(index);
    }
    let choices = formats
        .iter()
        .map(|mode| {
            format!(
                "{}x{}@{}",
                mode.format.width, mode.format.height, mode.format.fps
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
    textures: TexturePool,
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
        let current = qualified_format(&unsafe { reader.GetCurrentMediaType(stream) }?)?;
        if current != selected.format {
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
        Ok(Self {
            resources: Some(Resources {
                reader,
                _source: source,
                _manager: manager,
                textures: TexturePool::new(graphics.clone()),
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
                bail!("MF camera delivered no qualified GPU frame within 5 seconds");
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

    fn check_flags(&self, flags: u32, running: bool) -> Result<()> {
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
            let current = qualified_format(&unsafe { reader.GetCurrentMediaType(self.stream) }?)?;
            if running
                || current != self.format
                || flags
                    & (MF_SOURCE_READERF_NEWSTREAM.0 as u32
                        | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32)
                    != 0
            {
                bail!(
                    "camera.format changed (flags {flags:#x}, current {current:?}); reopen explicitly"
                );
            }
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
        let buffers = unsafe { sample.GetBufferCount() }?;
        if buffers != 1 {
            bail!("camera.frame: requires exactly one DXGI buffer, got {buffers}");
        }
        let buffer = unsafe { sample.GetBufferByIndex(0) }?;
        let dxgi: IMFDXGIBuffer = buffer
            .cast()
            .context("camera.frame: CPU/non-DXGI buffers are not supported")?;
        let mut raw = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }?;
        if raw.is_null() {
            bail!("camera.frame: null D3D11 texture");
        }
        // GetResource transferred this owned COM reference to the caller.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }?;
        let owner: ID3D11Device5 = unsafe { texture.GetDevice() }?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let resources = self.resources.as_mut().expect("open camera");
        if owner != *resources.graphics.raw()
            || desc.Format != DXGI_FORMAT_NV12
            || desc.Usage != D3D11_USAGE_DEFAULT
            || desc.CPUAccessFlags != 0
            || desc.MipLevels != 1
            || desc.ArraySize == 0
            || subresource >= desc.ArraySize
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
            || desc.Width < self.format.width
            || desc.Height < self.format.height
            || !desc.Width.is_multiple_of(2)
            || !desc.Height.is_multiple_of(2)
        {
            bail!(
                "camera.frame: requires same-device, GPU-only default-usage NV12 with a valid array subresource and dimensions; got {desc:?}, subresource {subresource}"
            );
        }
        // A failing pool call may already have queued CopySubresourceRegion
        // without a successful fence signal. Keep its source even on Err.
        resources.uncertain = Some(sample);
        let frame = resources.textures.copy_subresource(
            &texture,
            subresource,
            self.format.width,
            self.format.height,
        )?;
        let sample = resources.uncertain.take().expect("copy source retained");
        if let Some(frame) = &frame {
            count_fields(&self.state, interlaced, bottom_first);
            resources.pending.push_back(Pending {
                frame: frame.clone(),
                _sample: sample,
                submitted: Instant::now(),
            });
        } else {
            self.state.counts.pool_drops.fetch_add(1, Ordering::Relaxed);
        }
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
// MF_DEVSOURCE_ATTRIBUTE_FRAMESERVER_SHARE_MODE (Windows 11 26100). Only
// compressed MJPEG needs this fallback today: the native NV12 path above
// never downloads or software-converts its GPU samples.
struct SharedCamera {
    capture: MediaCapture,
    reader: Option<MediaFrameReader>,
    on_frame: Option<i64>,
    on_error: Option<i64>,
    error: Arc<Mutex<Option<String>>>,
    state: Arc<State>,
    device: CameraDevice,
    format: Option<CameraFormat>,
    output: Option<OutputPool>,
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
            output: None,
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
        let format = shared_format(&source.CurrentFormat()?)?;
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
        session.output = Some(OutputPool::new(graphics, format.width, format.height)?);
        session.format = Some(format);
        session
            .state
            .observed
            .media_type_interlace_mode
            .store(format.media_type_interlace_mode, Ordering::Relaxed);
        session
            .state
            .observed
            .shared_mjpg
            .store(true, Ordering::Relaxed);
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
        let factory: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
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
            let frame = self.decode(&factory, &reference);
            // WIC and the synchronous CPU upload are finished; nothing on the
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

    fn decode(
        &mut self,
        factory: &IWICImagingFactory,
        reference: &MediaFrameReference,
    ) -> Result<Option<(Frame, Time)>> {
        self.state
            .counts
            .received_frames
            .fetch_add(1, Ordering::Relaxed);
        let format = self.format.expect("shared format");
        if shared_format(&reference.Format()?)? != format {
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
        let buffer = reference.BufferMediaFrame()?.Buffer()?;
        let length = buffer.Length()? as usize;
        // Compressed input is bounded independently of what a corrupt JPEG
        // header claims. No allocation is sized from unvalidated JPEG data.
        if !(2..=64 * 1024 * 1024).contains(&length) {
            bail!("camera.frame: invalid MJPEG buffer length {length}");
        }
        let access: IBufferByteAccess = buffer.cast()?;
        let pointer = unsafe { access.Buffer() }?;
        if pointer.is_null() {
            bail!("camera.frame: null MJPEG buffer");
        }
        let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
        let stream = unsafe { factory.CreateStream() }?;
        // IWICStream borrows these bytes. The frame, buffer and access live
        // until decoding completes; declaration order releases WIC first.
        unsafe { stream.InitializeFromMemory(bytes) }?;
        let decoder = unsafe {
            factory.CreateDecoderFromStream(
                &stream,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )
        }?;
        let image = unsafe { decoder.GetFrame(0) }?;
        let (mut width, mut height) = (0, 0);
        unsafe { image.GetSize(&mut width, &mut height) }?;
        if (width, height) != (format.width, format.height) {
            bail!("camera.frame: JPEG dimensions {width}x{height} differ from {format:?}");
        }
        // WIC decodes the complete JPEG without scaling or changing row
        // order. JPEG's own color conversion produces BGRA; do not label it
        // as the native NV12 path's limited-range BT.709.
        let bgra = unsafe { WICConvertBitmapSource(&GUID_WICPixelFormat32bppBGRA, &image) }?;
        let mut decoded = Ok(());
        let frame = self
            .output
            .as_mut()
            .expect("shared output")
            .draw(&mut |pixels, stride| {
                decoded = unsafe { bgra.CopyPixels(std::ptr::null(), stride as u32, pixels) };
            })?;
        decoded.context("camera.frame: JPEG decode")?;
        if frame.is_some() {
            count_fields(&self.state, interlaced, bottom_first);
        } else {
            self.state.counts.pool_drops.fetch_add(1, Ordering::Relaxed);
        }
        Ok(frame.map(|frame| (frame, at)))
    }
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
    }
}

fn shared_format(format: &MediaFrameFormat) -> Result<CameraFormat> {
    let subtype = format.Subtype()?;
    if subtype != "MJPG" {
        bail!(
            "camera.format: shared source uses {subtype}; supported shared format is MJPG (native NV12 uses the GPU Source Reader)"
        );
    }
    let video = format.VideoFormat()?;
    let rate = format.FrameRate()?;
    let (numerator, denominator) = (rate.Numerator()?, rate.Denominator()?);
    let (width, height) = (video.Width()?, video.Height()?);
    if denominator == 0
        || !numerator.is_multiple_of(denominator)
        || !(1..=240).contains(&(numerator / denominator))
        || width == 0
        || height == 0
    {
        bail!(
            "camera.format: invalid shared dimensions/rate {width}x{height}@{numerator}/{denominator}"
        );
    }
    let properties = format.Properties()?;
    let interlace = if properties.HasKey(MF_MT_INTERLACE_MODE)? {
        properties
            .Lookup(MF_MT_INTERLACE_MODE)?
            .cast::<IPropertyValue>()?
            .GetUInt32()?
    } else {
        MFVideoInterlace_Unknown.0 as u32
    };
    Ok(CameraFormat {
        width,
        height,
        fps: numerator / denominator,
        media_type_interlace_mode: interlace,
    })
}
