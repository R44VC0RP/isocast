//! Media Foundation hardware encoders and the last-resort system encoder.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{EncodedVideo, LiveVideoSpec, Time, VideoEncoder};
use parking_lot::Mutex;
use windows::Win32::Foundation::{
    E_INVALIDARG, E_NOTIMPL, E_POINTER, S_OK, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::{GUID, Interface, PWSTR, Ref, implement};

use crate::convert::Converter;
use crate::encoder::Parameters;
use crate::h264;
use crate::native::{Apartment, Event};
use crate::{EncodeStats, Frame, GraphicsDevice, PixelFormat, VideoEncoding};

const IN_FLIGHT: usize = 8;
const FINISH_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfMode {
    Hardware,
    Software,
}

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// All COM/MFT operations stay on one private MTA worker, even if the
/// engine moves this handle between its own threads. Software is explicit:
/// it downloads NV12/P010, while hardware receives a DXGI texture sample.
pub struct MfEncoder {
    jobs: Option<mpsc::SyncSender<Job>>,
    worker: Option<JoinHandle<()>>,
    info: MfEncoderInfo,
    low_latency: bool,
    recording: bool,
    ready: VecDeque<EncodedVideo>,
    dropped: u64,
    native_dropped: Arc<AtomicU64>,
}

impl MfEncoder {
    pub fn new(device: &GraphicsDevice, spec: &LiveVideoSpec, mode: MfMode) -> Result<Self> {
        Self::with_format(device, spec, mode, VideoEncoding::H264, PixelFormat::Bgra8)
    }

    pub fn with_format(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        mode: MfMode,
        encoding: VideoEncoding,
        input: PixelFormat,
    ) -> Result<Self> {
        Self::open(device, spec, mode, encoding, input, None)
    }

    /// Recording at the nearest accepted native MF quality (0..=100), not
    /// macOS quality. `info()` reports both requested and actual quality.
    /// Ignores `spec.bitrate_kbps` and `spec.constant_bitrate`. Providers that
    /// cannot honor quality mode without a bitrate are rejected. Changes to
    /// recording settings, including fps, require opening a new encoder.
    pub fn with_quality(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        mode: MfMode,
        encoding: VideoEncoding,
        input: PixelFormat,
        quality: u32,
    ) -> Result<Self> {
        Self::open(device, spec, mode, encoding, input, Some(quality))
    }

    fn open(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        mode: MfMode,
        encoding: VideoEncoding,
        input: PixelFormat,
        quality: Option<u32>,
    ) -> Result<Self> {
        encoding.validate(input)?;
        if let Some(quality) = quality {
            if quality > 100 {
                bail!("video.quality: Media Foundation valid range is 0..=100");
            }
            validate_fps(spec.fps)?;
        } else {
            validate(spec.fps, spec.bitrate_kbps)?;
        }
        if spec.width == 0
            || spec.height == 0
            || !spec.width.is_multiple_of(2)
            || !spec.height.is_multiple_of(2)
            || spec.keyframe_interval == 0
        {
            bail!("video: MF requires positive even dimensions and keyframe_interval >= 1");
        }
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let (started, startup) = mpsc::sync_channel(1);
        let device = device.clone();
        let settings = *spec;
        let worker = std::thread::Builder::new()
            .name("Media Foundation encoder".into())
            .spawn(move || {
                let run =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _apartment = Apartment::new()?;
                        let _runtime = Runtime::new()?;
                        let mut session =
                            Session::open(&device, &settings, mode, encoding, input, quality)?;
                        if started.send(Ok(session.info.clone())).is_err() {
                            return Ok(());
                        }
                        while let Ok(job) = rx.recv() {
                            job(&mut session);
                        }
                        Ok(())
                    }));
                let error = match run {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("{error:#}"),
                    Err(_) => "MF encoder worker panicked".into(),
                };
                let _ = started.send(Err(error));
            })?;
        // The engine's bounded native-open workers own the startup deadline;
        // do not detach a new stuck COM worker on every retry.
        let info = match startup.recv() {
            Ok(Ok(info)) => info,
            result => {
                drop(tx);
                let _ = worker.join();
                bail!(
                    "encoder.mf: {}",
                    match result {
                        Ok(Err(error)) => error,
                        _ => "worker stopped during startup".into(),
                    }
                );
            }
        };
        Ok(Self {
            jobs: Some(tx),
            worker: Some(worker),
            info,
            low_latency: spec.low_latency,
            recording: quality.is_some(),
            ready: VecDeque::new(),
            dropped: 0,
            native_dropped: Arc::new(AtomicU64::new(0)),
        })
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        let losses = self.native_dropped.clone();
        self.jobs
            .as_ref()
            .context("MF encoder is closed")?
            .send(Box::new(move |session| {
                let result = f(session);
                losses.store(session.counts.dropped, Ordering::Release);
                let _ = tx.send(result);
            }))
            .map_err(|_| anyhow!("MF encoder worker stopped"))?;
        rx.recv()
            .context("MF encoder worker stopped during a request")?
    }

    pub fn info(&self) -> &MfEncoderInfo {
        &self.info
    }
    pub fn try_encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<bool> {
        let frame = frame.clone();
        self.call(move |session| {
            session.healthy()?;
            session.validate_frame(&frame, pts)?;
            let result = session.encode(&frame, pts, keyframe);
            if let Err(error) = &result {
                session.failed = Some(format!("{error:#}"));
            }
            result
        })
    }
    pub fn decoder_config(&self) -> Result<Vec<u8>> {
        self.call(|session| session.parameters.config())
    }
    pub fn stats(&self) -> Result<EncodeStats> {
        let mut stats = self.call(|session| Ok(session.stats()))?;
        stats.dropped += self.dropped;
        stats.ready_packets = self.ready.len();
        Ok(stats)
    }
    /// Finish encoding and return the remaining packets. On native failure,
    /// completed packets remain available through `receive`, followed by the
    /// sticky encoder error. A worker/RPC failure preserves packets already
    /// queued on this handle, but cannot recover an unsent worker-side prefix.
    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>> {
        let (packets, error) = self.call(|session| {
            let mut packets = Vec::new();
            let error = session.finish(&mut packets).err().map(|error| {
                // Return the completed prefix even when finalization fails;
                // only the error text, never a COM error, crosses the worker.
                let error = format!("{error:#}");
                session.failed.get_or_insert_with(|| error.clone());
                error
            });
            Ok((packets, error))
        })?;
        self.ready.extend(packets);
        if let Some(error) = error {
            bail!("{error}");
        }
        Ok(self.ready.drain(..).collect())
    }

    fn media_call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.call(move |session| {
            session.healthy()?;
            let result = f(session);
            if let Err(error) = &result {
                session.failed = Some(format!("{error:#}"));
            }
            result
        })
    }
}

impl VideoEncoder for MfEncoder {
    type Frame = Frame;
    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        if self.try_encode(frame, pts, keyframe)? {
            return Ok(());
        }
        if self.ready.len() < IN_FLIGHT {
            let timeout = if self.low_latency {
                Duration::ZERO
            } else {
                Duration::from_millis(400)
            };
            if let Some(packet) = self.media_call(move |session| {
                let packet = session.receive(timeout)?;
                // Consuming output can make the MFT request another input,
                // but BeginGetEvent delivers that credit on a work-queue hop.
                // Do not count this brief delivery lag as a dropped frame.
                session.wait_input_credit(Duration::from_millis(2))?;
                Ok(packet)
            })? {
                self.ready.push_back(packet);
            }
            if self.try_encode(frame, pts, keyframe)? {
                return Ok(());
            }
        }
        self.dropped += 1;
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.receive_timeout(Duration::ZERO)
    }
    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        if let Some(packet) = self.ready.pop_front() {
            return Ok(Some(packet));
        }
        self.media_call(move |session| session.receive(timeout))
    }
    fn dropped(&self) -> u64 {
        self.dropped + self.native_dropped.load(Ordering::Acquire)
    }
    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        if self.recording {
            bail!("encoder.mf: recording settings cannot change; open a new encoder");
        }
        let bits = validate(fps, bitrate_kbps)?;
        self.call(move |session| session.set_rate(fps, bits))
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Activation(IMFActivate);
impl Drop for Activation {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ShutdownObject() };
    }
}

struct EventState {
    wake: Arc<Event>,
    result: Mutex<Option<EventResult>>,
    stopping: AtomicBool,
}

struct EventResult(IMFAsyncResult);

// SAFETY: the MF event callback and our encoder worker run in the process
// MTA. Moving the result within that apartment needs no COM marshaling. It
// is transferred under the mutex and consumed only by EndGetEvent on the
// worker, never exposed to a caller's potentially different apartment.
unsafe impl Send for EventResult {}

#[implement(IMFAsyncCallback)]
struct EventCallback {
    state: Arc<EventState>,
}
impl IMFAsyncCallback_Impl for EventCallback_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let mut pending = self.state.result.lock();
        if !self.state.stopping.load(Ordering::Acquire) {
            *pending = Some(EventResult(
                result
                    .as_ref()
                    .ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?
                    .clone(),
            ));
            self.state.wake.signal();
        }
        Ok(())
    }
}

struct Events {
    generator: IMFMediaEventGenerator,
    callback: IMFAsyncCallback,
    state: Arc<EventState>,
}
impl Events {
    fn new(transform: &IMFTransform, wake: Arc<Event>) -> Result<Self> {
        let generator: IMFMediaEventGenerator = transform.cast()?;
        let state = Arc::new(EventState {
            wake,
            result: Mutex::new(None),
            stopping: AtomicBool::new(false),
        });
        let callback = EventCallback {
            state: state.clone(),
        }
        .into();
        unsafe { generator.BeginGetEvent(&callback, None) }?;
        Ok(Self {
            generator,
            callback,
            state,
        })
    }
    fn next(&self, timeout: Duration) -> Result<Option<IMFMediaEvent>> {
        if self.state.result.lock().is_none() && !timeout.is_zero() {
            wait(&self.state.wake, timeout)?;
        }
        let Some(result) = self.state.result.lock().take() else {
            return Ok(None);
        };
        let event = unsafe { self.generator.EndGetEvent(&result.0) }?;
        unsafe { self.generator.BeginGetEvent(&self.callback, None) }?;
        Ok(Some(event))
    }
    fn stop(&self) {
        let mut result = self.state.result.lock();
        self.state.stopping.store(true, Ordering::Release);
        result.take();
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Slot {
    texture: ID3D11Texture2D,
    free: AtomicBool,
    _device: GraphicsDevice,
}
#[implement(IMFAsyncCallback)]
struct Released {
    slot: Arc<Slot>,
    wake: Arc<Event>,
}
impl IMFAsyncCallback_Impl for Released_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(&self, _: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        self.slot.free.store(true, Ordering::Release);
        self.wake.signal();
        Ok(())
    }
}

struct Pending {
    pts: Time,
    sample_time: i64,
    submitted: Instant,
    _sample: IMFSample,
}

struct Session {
    activation: Activation,
    transform: IMFTransform,
    codec: ICodecAPI,
    _manager: Option<IMFDXGIDeviceManager>,
    events: Option<Events>,
    wake: Arc<Event>,
    converter: Converter,
    device: GraphicsDevice,
    slots: Vec<Arc<Slot>>,
    info: MfEncoderInfo,
    output_info: MFT_OUTPUT_STREAM_INFO,
    parameters: Parameters,
    input: PixelFormat,
    size: (u32, u32),
    fps: u32,
    recording: bool,
    need_input: u32,
    have_output: u32,
    drain_complete: bool,
    finishing: bool,
    finished: bool,
    opened: bool,
    deadline: Option<Instant>,
    pending: VecDeque<Pending>,
    last_pts: Option<Time>,
    counts: EncodeStats,
    failed: Option<String>,
}

impl Session {
    fn open(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        mode: MfMode,
        encoding: VideoEncoding,
        input: PixelFormat,
        quality: Option<u32>,
    ) -> Result<Self> {
        let mut failures = Vec::new();
        let hardware = mode == MfMode::Hardware;
        for activation in candidates(hardware, hardware.then(|| device.adapter_luid()), encoding)? {
            let label = name(&activation)?;
            match Self::open_one(
                device,
                spec,
                mode,
                (Activation(activation), label.clone()),
                encoding,
                input,
                quality,
            ) {
                Ok(session) => return Ok(session),
                Err(error) => failures.push(format!("{label}: {error:#}")),
            }
        }
        bail!(
            "no usable {mode:?} Media Foundation {encoding:?} encoder{}",
            if failures.is_empty() {
                " is installed".into()
            } else {
                format!(": {}", failures.join("; "))
            }
        )
    }

    fn open_one(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        mode: MfMode,
        candidate: (Activation, String),
        encoding: VideoEncoding,
        pixel_format: PixelFormat,
        quality: Option<u32>,
    ) -> Result<Self> {
        let (activation, label) = candidate;
        unsafe {
            let transform: IMFTransform = activation.0.ActivateObject()?;
            let attrs = transform.GetAttributes()?;
            let asynchronous = attribute(&attrs, &MF_TRANSFORM_ASYNC)? != 0;
            let d3d11 = attribute(&attrs, &MF_SA_D3D11_AWARE)? != 0;
            if asynchronous {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let (mut inputs, mut outputs) = (0, 0);
            transform.GetStreamCount(&mut inputs, &mut outputs)?;
            if inputs != 1 || outputs != 1 {
                bail!("video encoder must expose one input and one output stream");
            }
            let (mut input, mut output) = ([0], [0]);
            if let Err(error) = transform.GetStreamIDs(&mut input, &mut output)
                && error.code() != E_NOTIMPL
            {
                return Err(error.into());
            }
            if input[0] != 0 || output[0] != 0 {
                bail!("video encoder has unsupported nonzero stream ids");
            }
            let manager = if mode == MfMode::Hardware {
                if !asynchronous || !d3d11 {
                    bail!(
                        "hardware transform must support async D3D11 texture input; no hidden CPU fallback"
                    );
                }
                let mut token = 0;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.context("MF returned no DXGI device manager")?;
                manager.ResetDevice(device.raw(), token)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                Some(manager)
            } else {
                None
            };
            let codec: ICodecAPI = transform.cast()?;
            let b_picture_control = codec
                .IsSupported(&CODECAPI_AVEncMPVDefaultBPictureCount)
                .is_ok();
            if b_picture_control {
                set(
                    &codec,
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    VARIANT::from(0u32),
                )
                .context("disable B-frame reordering")?;
            }
            set(&codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true))
                .context("enable low-latency encoding")?;
            if encoding == VideoEncoding::H264
                && codec.IsSupported(&CODECAPI_AVEncH264CABACEnable).is_ok()
            {
                set(&codec, &CODECAPI_AVEncH264CABACEnable, VARIANT::from(true))?;
            }
            if let Some(quality) = quality {
                set_required(
                    &codec,
                    &CODECAPI_AVEncCommonRateControlMode,
                    eAVEncCommonRateControlMode_Quality.0 as u32,
                    "rate control mode",
                )?;
                set_required(&codec, &CODECAPI_AVEncCommonQuality, quality, "quality")?;
            } else {
                set(
                    &codec,
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(if spec.constant_bitrate { 0u32 } else { 1u32 }),
                )?;
            }
            set(
                &codec,
                &CODECAPI_AVEncMPVGOPSize,
                VARIANT::from(spec.keyframe_interval),
            )?;
            let bits = if quality.is_none() {
                let bits = validate(spec.fps, spec.bitrate_kbps)?;
                set(
                    &codec,
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(bits),
                )?;
                if !spec.constant_bitrate {
                    set(&codec, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bits))?;
                }
                Some(bits)
            } else {
                None
            };
            let output = media_type(spec, subtype(encoding))?;
            if let Some(bits) = bits {
                output.SetUINT32(&MF_MT_AVG_BITRATE, bits)?;
            }
            let profile = match encoding {
                VideoEncoding::H264 => eAVEncH264VProfile_High.0 as u32,
                VideoEncoding::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
                VideoEncoding::Hevc10 => eAVEncH265VProfile_Main_420_10.0 as u32,
            };
            output.SetUINT32(&MF_MT_VIDEO_PROFILE, profile)?;
            transform
                .SetOutputType(0, &output, 0)
                .context(if quality.is_some() {
                    "MF recording provider rejected quality output without MF_MT_AVG_BITRATE"
                } else {
                    "set MF output type"
                })?;
            let bytes_per_pixel = if encoding.bit_depth() == 10 { 2 } else { 1 };
            let input = media_type(
                spec,
                if bytes_per_pixel == 2 {
                    MFVideoFormat_P010
                } else {
                    MFVideoFormat_NV12
                },
            )?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            input.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
            input.SetUINT32(
                &MF_MT_DEFAULT_STRIDE,
                spec.width
                    .checked_mul(bytes_per_pixel)
                    .context("video stride overflow")?,
            )?;
            input.SetUINT32(
                &MF_MT_SAMPLE_SIZE,
                spec.width
                    .checked_mul(spec.height)
                    .and_then(|n| n.checked_mul(3))
                    .and_then(|n| n.checked_mul(bytes_per_pixel))
                    .map(|n| n / 2)
                    .context("video dimensions overflow")?,
            )?;
            transform.SetInputType(0, &input, 0)?;
            let actual_quality = quality
                .map(|quality| negotiate_quality(&transform, &codec, &output, &input, quality))
                .transpose()?;
            let wake = Arc::new(Event::new()?);
            let events = if asynchronous {
                Some(Events::new(&transform, wake.clone())?)
            } else {
                None
            };
            let converter = Converter::with_format(
                device,
                spec.width,
                spec.height,
                spec.fps,
                pixel_format,
                encoding.bit_depth(),
            )?;
            let output_info = transform.GetOutputStreamInfo(0)?;
            let mut session = Self {
                activation,
                transform,
                codec,
                _manager: manager,
                events,
                wake,
                converter,
                device: device.clone(),
                slots: Vec::new(),
                info: MfEncoderInfo {
                    name: label,
                    hardware: mode == MfMode::Hardware,
                    asynchronous,
                    d3d11,
                    b_picture_control,
                    encoding,
                    requested_quality: quality,
                    quality: actual_quality,
                },
                output_info,
                parameters: Parameters::new(encoding),
                input: pixel_format,
                size: (spec.width, spec.height),
                fps: spec.fps,
                recording: quality.is_some(),
                need_input: 0,
                have_output: 0,
                drain_complete: false,
                finishing: false,
                finished: false,
                opened: false,
                deadline: None,
                pending: VecDeque::new(),
                last_pts: None,
                counts: EncodeStats::default(),
                failed: None,
            };
            session.sequence_header()?;
            session
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            session
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            if session.info.asynchronous {
                let deadline = Instant::now() + FINISH_WAIT;
                while session.need_input == 0 {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        bail!("MF encoder did not request its first input within 5 seconds");
                    }
                    session.events(left)?;
                }
            }
            if let Some(quality) = session.info.quality {
                let actual = read_quality(&session.codec)?;
                if actual != quality {
                    bail!(
                        "MF recording quality: selected {quality}, provider returned {actual} after streaming began"
                    );
                }
            }
            session.opened = true;
            Ok(session)
        }
    }

    fn sequence_header(&mut self) -> Result<()> {
        let ty = unsafe { self.transform.GetOutputCurrentType(0) }?;
        let size = match unsafe { ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
            Ok(size) => size,
            Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if size == 0 {
            return Ok(());
        }
        if size > 65536 {
            bail!("MF video configuration is too large");
        }
        let mut bytes = vec![0; size as usize];
        unsafe { ty.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut bytes, None) }?;
        if self.info.encoding == VideoEncoding::H264 {
            // Preserve the H.264 path's tolerance for incomplete open-time
            // headers; in-band SPS/PPS can complete them before output.
            let sets = h264::parameters(&h264::annex_b(&bytes)?);
            if sets.sps.is_empty() || sets.pps.is_empty() {
                return Ok(());
            }
        }
        self.parameters.headers(&bytes)?;
        Ok(())
    }

    fn events(&mut self, timeout: Duration) -> Result<()> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        let mut timeout = timeout;
        for _ in 0..256 {
            let Some(event) = events.next(timeout)? else {
                return Ok(());
            };
            timeout = Duration::ZERO;
            unsafe {
                event.GetStatus()?.ok()?;
            }
            match unsafe { event.GetType() }? as i32 {
                n if n == METransformNeedInput.0 => {
                    self.need_input = self
                        .need_input
                        .checked_add(1)
                        .context("MF input-credit overflow")?
                }
                n if n == METransformHaveOutput.0 => {
                    self.have_output = self
                        .have_output
                        .checked_add(1)
                        .context("MF output-credit overflow")?
                }
                n if n == METransformDrainComplete.0 => self.drain_complete = true,
                _ => {}
            }
        }
        bail!("MF encoder emitted an unbounded event burst")
    }

    fn wait_input_credit(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while self.info.asynchronous && self.need_input == 0 && self.pending.len() < IN_FLIGHT {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.events(left)?;
        }
        Ok(())
    }

    fn validate_frame(&self, frame: &Frame, pts: Time) -> Result<()> {
        if self.finishing {
            bail!("MF encoder has begun finishing");
        }
        if pts.as_nanos() < 0 || self.last_pts.is_some_and(|last| pts <= last) {
            bail!("video.pts: must be nonnegative and increase between accepted frames");
        }
        if self.size != (frame.width(), frame.height()) || !self.device.owns(frame) {
            bail!("video.frame: must match the MF encoder size and D3D11 device");
        }
        if frame.pixel_format() != self.input {
            bail!(
                "video.input: MF requires {:?}, got {:?}",
                self.input,
                frame.pixel_format()
            );
        }
        Ok(())
    }

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<bool> {
        self.events(Duration::ZERO)?;
        if self.pending.len() >= IN_FLIGHT || (self.info.asynchronous && self.need_input == 0) {
            return Ok(false);
        }
        let slot = match self
            .slots
            .iter()
            .find(|slot| slot.free.load(Ordering::Acquire))
        {
            Some(slot) => slot.clone(),
            None if self.slots.len() == IN_FLIGHT => return Ok(false),
            None => {
                let slot = Arc::new(Slot {
                    texture: self.converter.create_texture()?,
                    free: AtomicBool::new(true),
                    _device: self.device.clone(),
                });
                self.slots.push(slot.clone());
                slot
            }
        };
        let submitted = Instant::now();
        self.converter.convert(frame, &slot.texture)?;
        let buffer = if self.info.hardware {
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &slot.texture, 0, false) }?
        } else {
            // The sole CPU pixel download path is explicitly labelled software.
            let bytes = self.converter.readback(&slot.texture)?;
            let buffer = unsafe { MFCreateMemoryBuffer(bytes.len() as u32) }?;
            let mut data = std::ptr::null_mut();
            unsafe {
                buffer.Lock(&mut data, None, None)?;
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(bytes.len() as u32)?;
            }
            buffer
        };
        let tracked = unsafe { MFCreateTrackedSample() }?;
        let sample: IMFSample = tracked.cast()?;
        let sample_time = pts.as_nanos() / 100;
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(sample_time)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.fps))?;
            if keyframe || self.last_pts.is_none() {
                set(
                    &self.codec,
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    VARIANT::from(1u32),
                )?;
            }
            if self.info.hardware {
                let callback: IMFAsyncCallback = Released {
                    slot: slot.clone(),
                    wake: self.wake.clone(),
                }
                .into();
                // Only the sample's final native release makes a GPU slot free,
                // not merely a matching output packet arriving.
                slot.free.store(false, Ordering::Release);
                if let Err(error) = tracked.SetAllocator(&callback, None) {
                    slot.free.store(true, Ordering::Release);
                    return Err(error.into());
                }
            }
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(error) if error.code() == MF_E_NOTACCEPTING => return Ok(false),
                Err(error) => return Err(error).context("MF ProcessInput"),
            }
        }
        if self.info.asynchronous {
            self.need_input -= 1;
        }
        self.pending.push_back(Pending {
            pts,
            sample_time,
            submitted,
            _sample: sample,
        });
        self.counts.accepted += 1;
        self.counts.last_submit_latency = submitted.elapsed();
        self.last_pts = Some(pts);
        Ok(true)
    }

    fn output(&mut self) -> Result<Option<EncodedVideo>> {
        if self.info.asynchronous && self.have_output == 0 {
            return Ok(None);
        }
        let supplied = self.output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let sample = if supplied {
            None
        } else {
            let size = self.output_info.cbSize;
            if size == 0 || size as usize > h264::MAX_PACKET {
                bail!("MF output buffer size {size} is invalid");
            }
            let sample = unsafe { MFCreateSample() }?;
            let buffer = unsafe { MFCreateMemoryBuffer(size) }?;
            unsafe { sample.AddBuffer(&buffer) }?;
            Some(sample)
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut output.pEvents) });
        if self.info.asynchronous {
            self.have_output -= 1;
        }
        match result {
            Err(error)
                if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT && !self.info.asynchronous =>
            {
                if self.finishing {
                    self.drain_complete = true;
                }
                return Ok(None);
            }
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let ty = unsafe { self.transform.GetOutputAvailableType(0, 0) }?;
                unsafe { self.transform.SetOutputType(0, &ty, 0) }?;
                self.output_info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
                self.sequence_header()?;
                return Ok(None);
            }
            Err(error) => return Err(error).context("MF ProcessOutput"),
            Ok(()) => {}
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }?;
        let packet = if data.is_null() || length == 0 || length as usize > h264::MAX_PACKET {
            Err(anyhow!("MF returned invalid encoded packet size {length}"))
        } else {
            self.parameters.packet(
                unsafe { std::slice::from_raw_parts(data, length as usize) },
                self.pending
                    .front()
                    .map_or(Time::from_nanos(0), |pending| pending.pts),
            )
        };
        unsafe { buffer.Unlock() }?;
        // Hardware MFTs may emit configuration/SEI separately from a picture.
        // Such a sample need not have a timestamp and consumes no input.
        let Some(packet) = packet? else {
            return Ok(None);
        };
        let at = unsafe { sample.GetSampleTime() }?;
        let pending = self
            .pending
            .front()
            .context("MF returned a picture without an input")?;
        if at != pending.sample_time {
            bail!(
                "MF reordered or changed a picture timestamp: expected {}, got {at}",
                pending.sample_time
            );
        }
        if let Ok(dts) = unsafe { sample.GetUINT64(&MFSampleExtension_DecodeTimestamp) }
            && dts as i64 != at
        {
            bail!("MF encoder returned reordered pictures");
        }
        let pending = self.pending.pop_front().expect("checked above");
        self.counts.last_receive_latency = pending.submitted.elapsed();
        self.counts.completed += 1;
        Ok(Some(packet))
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let began = Instant::now();
        loop {
            self.events(Duration::ZERO)?;
            if let Some(packet) = self.output()? {
                return Ok(Some(packet));
            }
            let remaining = timeout.saturating_sub(began.elapsed());
            if remaining.is_zero()
                || (!self.info.asynchronous && !self.finishing)
                || self.drain_complete
            {
                return Ok(None);
            }
            self.events(remaining)?;
        }
    }

    fn set_rate(&mut self, fps: u32, bits: u32) -> Result<()> {
        if self.recording {
            bail!("encoder.mf: recording settings cannot change; open a new encoder");
        }
        self.healthy()?;
        if self.finishing {
            bail!("MF encoder has begun finishing");
        }
        if fps != self.fps {
            set(
                &self.codec,
                &CODECAPI_AVEncVideoOutputFrameRate,
                VARIANT::from((u64::from(fps) << 32) | 1),
            )
            .context("MF encoder does not support a live frame-rate change")?;
            self.fps = fps;
        }
        set(
            &self.codec,
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(bits),
        )
    }

    fn stats(&self) -> EncodeStats {
        EncodeStats {
            in_flight: self.pending.len(),
            ..self.counts
        }
    }

    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.failed {
            bail!("MF encoder failed: {error}");
        }
        Ok(())
    }

    fn finish(&mut self, packets: &mut Vec<EncodedVideo>) -> Result<()> {
        if self.finished {
            return self.healthy();
        }
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + FINISH_WAIT);
        if !self.finishing {
            unsafe {
                self.transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                self.transform
                    .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
            }
            self.finishing = true;
            self.need_input = 0;
        }
        while !self.drain_complete || self.have_output > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("MF encoder did not drain within 5 seconds");
            }
            if let Some(packet) = self.receive(left)? {
                packets.push(packet);
            }
        }
        if !self.pending.is_empty() {
            let count = self.pending.len();
            self.counts.dropped += count as u64;
            self.pending.clear();
            self.failed = Some(format!("MF drain lost {count} submitted pictures"));
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
        }?;
        self.finished = true;
        self.healthy()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A candidate rejected during startup has never accepted an input.
        // Shutdown it directly, without another drain deadline per fallback.
        if self.opened
            && let Err(error) = self.finish(&mut Vec::new())
        {
            eprintln!("MF encoder teardown: {error:#}");
        }
        if let Some(events) = &self.events {
            events.stop();
        }
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
        // Activation shutdown cancels the event subscription. Tracked samples
        // retain their private texture/device until all native users release.
        let _ = &self.activation;
    }
}

fn validate_fps(fps: u32) -> Result<()> {
    if !(1..=240).contains(&fps) {
        bail!("video.fps: valid range is 1..=240");
    }
    Ok(())
}
fn validate(fps: u32, kbps: u32) -> Result<u32> {
    validate_fps(fps)?;
    kbps.checked_mul(1000)
        .filter(|bits| *bits > 0)
        .context("video.bitrate_kbps: must be positive and fit in 32-bit bits/second")
}
fn set(codec: &ICodecAPI, property: &GUID, value: VARIANT) -> Result<()> {
    unsafe { codec.SetValue(property, &value) }
        .with_context(|| format!("MF codec property {property:?}"))
}
fn set_required(codec: &ICodecAPI, property: &GUID, value: u32, name: &str) -> Result<()> {
    // The generated IsSupported wrapper folds S_FALSE into Ok(()), but the
    // codec contract requires exactly S_OK to mean the property is supported.
    let supported = unsafe { (codec.vtable().IsSupported)(codec.as_raw(), property) };
    if supported != S_OK {
        bail!("MF recording {name}: property is not supported ({supported:?})");
    }
    set(codec, property, VARIANT::from(value))
        .with_context(|| format!("MF recording {name}: provider rejected {value}"))
}
fn verify_value(codec: &ICodecAPI, property: &GUID, expected: u32, name: &str) -> Result<()> {
    let value = read_value(codec, property, name)?;
    if value != expected {
        bail!("MF recording {name}: requested {expected}, provider returned {value}");
    }
    Ok(())
}
fn read_value(codec: &ICodecAPI, property: &GUID, name: &str) -> Result<u32> {
    let value = unsafe { codec.GetValue(property) }
        .with_context(|| format!("MF recording {name}: cannot read back the value"))?;
    if value.vt() != VT_UI4 {
        bail!("MF recording {name}: readback is not a UINT32");
    }
    u32::try_from(&value).with_context(|| format!("MF recording {name}: readback is not a UINT32"))
}
fn read_quality(codec: &ICodecAPI) -> Result<u32> {
    verify_value(
        codec,
        &CODECAPI_AVEncCommonRateControlMode,
        eAVEncCommonRateControlMode_Quality.0 as u32,
        "rate control mode",
    )?;
    let quality = read_value(codec, &CODECAPI_AVEncCommonQuality, "quality")?;
    if quality > 100 {
        bail!("MF recording quality: provider returned {quality}, outside 0..=100");
    }
    Ok(quality)
}
fn negotiate_quality(
    transform: &IMFTransform,
    codec: &ICodecAPI,
    output: &IMFMediaType,
    input: &IMFMediaType,
    requested: u32,
) -> Result<u32> {
    // Read only after media-type negotiation: some MFTs echo SetValue until
    // the types are applied, then quantize. Exact providers need no retyping.
    let mut best = read_quality(codec)?;
    if best == requested {
        return Ok(best);
    }
    let mut command = requested;
    let apply = |quality| -> windows::core::Result<()> {
        unsafe {
            codec.SetValue(&CODECAPI_AVEncCommonQuality, &VARIANT::from(quality))?;
            transform.SetOutputType(0, output, 0)?;
            transform.SetInputType(0, input, 0)
        }
    };
    // Range/value enumeration is not implemented by every MFT. The entire
    // native domain is only 101 commands; search once, before streaming, and
    // keep the initial native selection on ties. Never cache across sessions.
    for candidate in 0..=100u32 {
        match apply(candidate) {
            Ok(()) => {}
            Err(error) if error.code() == E_INVALIDARG => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("MF recording quality: negotiate {candidate}"));
            }
        }
        let actual = read_quality(codec)?;
        if actual.abs_diff(requested) < best.abs_diff(requested) {
            best = actual;
            command = candidate;
            if best == requested {
                break;
            }
        }
    }
    apply(command)
        .with_context(|| format!("MF recording quality: pin selected command {command}"))?;
    let actual = read_quality(codec)?;
    if actual != best {
        bail!(
            "MF recording quality: unstable selection for requested {requested}; command {command} produced {best}, then {actual}"
        );
    }
    Ok(actual)
}
fn attribute(attrs: &IMFAttributes, key: &GUID) -> Result<u32> {
    match unsafe { attrs.GetUINT32(key) } {
        Ok(value) => Ok(value),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(0),
        Err(error) => Err(error.into()),
    }
}
fn media_type(spec: &LiveVideoSpec, subtype: GUID) -> Result<IMFMediaType> {
    unsafe {
        let ty = MFCreateMediaType()?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        ty.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(spec.width) << 32) | u64::from(spec.height),
        )?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(spec.fps) << 32) | 1)?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(ty)
    }
}
fn wait(event: &Event, timeout: Duration) -> Result<()> {
    match unsafe {
        WaitForSingleObject(
            event.handle(),
            timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32,
        )
    } {
        WAIT_OBJECT_0 | WAIT_TIMEOUT => Ok(()),
        _ => Err(windows::core::Error::from_thread().into()),
    }
}

#[derive(Debug, Clone)]
pub struct MfEncoderInfo {
    pub name: String,
    pub hardware: bool,
    pub asynchronous: bool,
    pub d3d11: bool,
    /// Some hardware MFTs expose low-latency control but not B-picture count.
    /// Output timestamps are checked for reordering in either case.
    pub b_picture_control: bool,
    pub encoding: VideoEncoding,
    /// Requested native quality; None for live encoding or enumeration.
    pub requested_quality: Option<u32>,
    /// Actual selected native quality; None for live encoding or enumeration.
    pub quality: Option<u32>,
}

pub(crate) struct Runtime;

impl Runtime {
    pub(crate) fn new() -> Result<Self> {
        // Windows 10 retains a thread-pool factory and 13 associated handles
        // each time an activated MF runtime is shut down and started again.
        // Keep ONE runtime for the process, as we already do for the WinRT
        // factory MTA. Transforms, samples and worker threads still close for
        // each encoder; runtime resources are reclaimed when the process exits.
        static STARTED: OnceLock<windows::core::Result<()>> = OnceLock::new();
        STARTED
            .get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
            .as_ref()
            .map_err(|error| anyhow!("could not start Media Foundation: {error}"))?;
        Ok(Self)
    }
}

fn subtype(encoding: VideoEncoding) -> GUID {
    if encoding == VideoEncoding::H264 {
        MFVideoFormat_H264
    } else {
        MFVideoFormat_HEVC
    }
}

fn candidates(
    hardware: bool,
    adapter: Option<(u32, i32)>,
    encoding: VideoEncoding,
) -> Result<Vec<IMFActivate>> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(encoding),
    };
    let flags = if hardware {
        MFT_ENUM_FLAG_HARDWARE
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_LOCALMFT
    };
    let mut list = std::ptr::null_mut();
    let mut count = 0;
    let attributes = if let Some((low, high)) = adapter {
        let mut attributes = None;
        unsafe { MFCreateAttributes(&mut attributes, 1) }?;
        let attributes = attributes.context("MF returned no adapter attributes")?;
        let mut luid = [0; 8];
        luid[..4].copy_from_slice(&low.to_le_bytes());
        luid[4..].copy_from_slice(&high.to_le_bytes());
        unsafe { attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, &luid) }?;
        Some(attributes)
    } else {
        None
    };
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output),
            attributes.as_ref(),
            &mut list,
            &mut count,
        )
    }?;
    let mut result = Vec::new();
    if !list.is_null() {
        // Move each owned COM reference out before freeing the COM array.
        for item in unsafe { std::slice::from_raw_parts_mut(list, count as usize) } {
            if let Some(item) = item.take() {
                result.push(item);
            }
        }
        unsafe { CoTaskMemFree(Some(list.cast())) };
    }
    Ok(result)
}

fn name(activation: &IMFActivate) -> Result<String> {
    let mut value = PWSTR::null();
    let mut len = 0;
    unsafe { activation.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len) }?;
    let result = if value.is_null() {
        String::new()
    } else {
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value.0, len as usize) })
    };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    Ok(result)
}

/// Enumerate and inspect H.264 transforms on a private COM thread. This
/// never opens a capture device, reads a user file or prompts for permission.
pub fn mf_encoders() -> Result<Vec<MfEncoderInfo>> {
    mf_encoders_for(VideoEncoding::H264)
}

/// The result describes installed transforms, not successful bit-depth
/// negotiation or available sessions; opening still checks those properties.
pub fn mf_encoders_for(encoding: VideoEncoding) -> Result<Vec<MfEncoderInfo>> {
    std::thread::Builder::new()
        .name("MF encoder discovery".into())
        .spawn(move || -> Result<_> {
            let _apartment = Apartment::new()?;
            let _runtime = Runtime::new()?;
            let mut result = Vec::new();
            for hardware in [true, false] {
                for activation in candidates(hardware, None, encoding)? {
                    let name = name(&activation)?;
                    let transform: IMFTransform = unsafe { activation.ActivateObject() }?;
                    let attributes = unsafe { transform.GetAttributes() }?;
                    let info = MfEncoderInfo {
                        name,
                        hardware,
                        asynchronous: unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }
                            .unwrap_or(0)
                            != 0,
                        d3d11: unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0)
                            != 0,
                        b_picture_control: transform.cast::<ICodecAPI>().is_ok_and(|codec| {
                            unsafe { codec.IsSupported(&CODECAPI_AVEncMPVDefaultBPictureCount) }
                                .is_ok()
                        }),
                        encoding,
                        requested_quality: None,
                        quality: None,
                    };
                    unsafe { activation.ShutdownObject() }?;
                    result.push(info);
                }
            }
            Ok(result)
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("MF discovery worker panicked"))?
        .context("enumerate Media Foundation video encoders")
}
