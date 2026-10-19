//! Event-driven shared WASAPI capture. COM objects and packet leases stay on
//! one worker; only copied samples and endpoint metadata leave its apartment.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{AudioSink, SAMPLE_RATE, Time};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{
    E_INVALIDARG, ERROR_NOT_FOUND, PROPERTYKEY, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AudioSessionDisconnectReason, AudioSessionState, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow,
    ERole, IAudioCaptureClient, IAudioClient, IAudioSessionControl, IAudioSessionEvents,
    IAudioSessionEvents_Impl, IMMDevice, IMMDeviceEnumerator, IMMEndpoint, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0, eCapture, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
use windows::core::{BOOL, BSTR, GUID, HRESULT, Interface, PCWSTR, PWSTR, implement};

use crate::native::{Apartment, Event};

const MAX_FRAMES: u32 = 48_000;
const MAX_CHANNELS: u32 = 32;

/// Active endpoint metadata. Channels and rate describe the native mix, not
/// the selected channels delivered by a capture (always 48 kHz).
#[derive(Clone, Debug)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub input: bool,
    pub default: bool,
    pub channels: u32,
    pub sample_rate: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct AudioCaptureStats {
    /// Acquired packets, including any rejected before delivery.
    pub packets: u64,
    /// Frames passed to sample callbacks that returned normally.
    pub delivered_frames: u64,
    /// Acquired silent frames, including any rejected before delivery.
    pub silent_frames: u64,
    /// Includes a first-packet flag; not itself a missing-frame estimate.
    pub discontinuities: u64,
    /// Exact gaps at native 48 kHz only; never synthesized as silence. With
    /// resampling, uncertain loss stops capture rather than inventing a count.
    pub gap_frames: u64,
    /// Fatal invalid/uncertain timing, including resampled discontinuities.
    /// Zero gap_frames does NOT imply no loss when this counter is nonzero.
    pub timestamp_errors: u64,
}

struct State {
    stop: Event,
    data: Event,
    stopping: AtomicBool,
    invalidated: AtomicU32,
    packets: AtomicU64,
    delivered_frames: AtomicU64,
    silent_frames: AtomicU64,
    discontinuities: AtomicU64,
    gap_frames: AtomicU64,
    timestamp_errors: AtomicU64,
}

impl State {
    fn check_device(&self) -> Result<()> {
        match self.invalidated.load(Ordering::Acquire) {
            0 => Ok(()),
            1 => bail!("selected audio endpoint was removed or became inactive"),
            2 => bail!("default eConsole audio endpoint changed; reopen capture explicitly"),
            _ => bail!("audio capture session disconnected; reopen capture explicitly"),
        }
    }

    fn invalidate(&self, reason: u32) {
        let _ = self
            .invalidated
            .compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire);
        self.data.signal();
    }
}

/// Drop signals and joins the worker, including native Stop. Drivers and user
/// callbacks can block; there is no hard wall-clock startup or stop deadline.
/// Dropping from its own callback requests stop instead of attempting a
/// self-join; the worker cleans up when that callback returns.
pub struct AudioCapture {
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
    device: AudioDevice,
}

impl AudioCapture {
    pub fn device(&self) -> &AudioDevice {
        &self.device
    }

    pub fn stats(&self) -> AudioCaptureStats {
        AudioCaptureStats {
            packets: self.state.packets.load(Ordering::Relaxed),
            delivered_frames: self.state.delivered_frames.load(Ordering::Relaxed),
            silent_frames: self.state.silent_frames.load(Ordering::Relaxed),
            discontinuities: self.state.discontinuities.load(Ordering::Relaxed),
            gap_frames: self.state.gap_frames.load(Ordering::Relaxed),
            timestamp_errors: self.state.timestamp_errors.load(Ordering::Relaxed),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        self.state.stop.signal();
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
        // A callback may itself drop the handle. It cannot join itself; the
        // worker observes stopping before any further callback and returns.
    }
}

/// Enumerates active inputs and outputs on a private, joined COM thread. No
/// streams are initialized or started, and no Windows settings are changed.
/// Unreadable/unusable endpoints (or failed flows) are omitted with stderr
/// diagnostics. If the default marker cannot be read, it is left false with
/// a diagnostic; capture of a requested default still resolves it strictly.
pub fn audio_devices() -> Result<Vec<AudioDevice>> {
    thread::Builder::new()
        .name("Windows audio devices".into())
        .spawn(|| {
            let _apartment = Apartment::new().map_err(|error| format!("{error:#}"))?;
            let result = (|| -> Result<_> {
                let enumerator = enumerator()?;
                let mut devices = Vec::new();
                for flow in [eCapture, eRender] {
                    let identities = match endpoint_identities(&enumerator, flow) {
                        Ok(identities) => identities,
                        Err(error) => {
                            eprintln!("Windows audio devices: omitting flow {}: {error:#}", flow.0);
                            continue;
                        }
                    };
                    let default = reported_default_id(&enumerator, flow);
                    for identity in identities {
                        let is_default = default.as_deref() == Some(identity.id.as_str());
                        match open_endpoint(identity, flow, is_default) {
                            Ok(endpoint) => devices.push(endpoint.device),
                            Err(error) => {
                                eprintln!("Windows audio devices: omitting endpoint: {error:#}")
                            }
                        }
                    }
                }
                Ok(devices)
            })();
            // Even native error-info interfaces are formatted/dropped here.
            result.map_err(|error| format!("Windows audio devices: {error:#}"))
        })?
        .join()
        .map_err(|_| anyhow!("Windows audio device enumeration panicked"))?
        .map_err(anyhow::Error::msg)
}

/// Resolve only the selected microphone and validate its channel selection.
/// Returns native mix metadata and the channel count capture would deliver.
/// No stream is initialized/started and no permission request is made. This
/// is not a permission grant or a guarantee that a later capture will start.
pub fn probe_microphone(device: &str, channels: Option<&[u32]>) -> Result<(AudioDevice, u32)> {
    validate_channel_count(channels)?;
    probe_audio(device, channels.map(<[u32]>::to_vec), eCapture)
}

/// Query the default eConsole render endpoint and its stereo loopback delivery.
/// Like `probe_microphone`, this reads metadata only, without starting audio.
pub fn probe_loopback() -> Result<(AudioDevice, u32)> {
    probe_audio("default", None, eRender)
}

fn probe_audio(
    query: &str,
    channels: Option<Vec<u32>>,
    flow: EDataFlow,
) -> Result<(AudioDevice, u32)> {
    let query = query.to_owned();
    thread::Builder::new()
        .name("Windows audio probe".into())
        .spawn(move || {
            let _apartment = Apartment::new().map_err(|error| format!("{error:#}"))?;
            let result = (|| -> Result<_> {
                let enumerator = enumerator()?;
                let endpoint = selected_endpoint(&enumerator, flow, &query)?;
                let (_, pick) = channel_selection(&endpoint.device, channels.as_deref())?;
                Ok((endpoint.device, pick.len() as u32))
            })();
            // Format/drop even native error-info interfaces before leaving
            // this apartment; only owned metadata or text crosses the thread.
            result.map_err(|error| format!("Windows audio probe: {error:#}"))
        })?
        .join()
        .map_err(|_| anyhow!("Windows audio probe panicked"))?
        .map_err(anyhow::Error::msg)
}

fn validate_channel_count(channels: Option<&[u32]>) -> Result<()> {
    if channels.is_some_and(|channels| channels.is_empty() || channels.len() > 2) {
        bail!("microphone.channels: select one or two distinct 1-based input channels");
    }
    Ok(())
}

/// Select one or two 1-based microphone channels, defaulting to the first two
/// (or its single mono channel). The returned count is the delivered count.
pub fn capture_microphone(
    device: &str,
    channels: Option<&[u32]>,
    sink: AudioSink,
    thread_start: fn(),
) -> Result<(AudioCapture, u32)> {
    validate_channel_count(channels)?;
    capture(
        device,
        channels.map(<[u32]>::to_vec),
        eCapture,
        sink,
        thread_start,
    )
}

/// Capture a render endpoint's mix as stereo, using WASAPI's channel matrix.
pub fn capture_loopback(
    device: &str,
    sink: AudioSink,
    thread_start: fn(),
) -> Result<(AudioCapture, u32)> {
    capture(device, None, eRender, sink, thread_start)
}

pub fn capture_system_audio(
    exclude: &[String],
    sink: AudioSink,
    exclude_own: bool,
    thread_start: fn(),
) -> Result<(AudioCapture, u32)> {
    if !exclude.is_empty() || exclude_own {
        bail!(
            "system_audio.exclude/exclude_own: process exclusion is not supported by this Windows endpoint-loopback backend; use exclude=[] and exclude_own=false"
        );
    }
    capture_loopback("default", sink, thread_start)
}

fn capture(
    query: &str,
    channels: Option<Vec<u32>>,
    flow: EDataFlow,
    sink: AudioSink,
    thread_start: fn(),
) -> Result<(AudioCapture, u32)> {
    let state = Arc::new(State {
        stop: Event::new()?,
        data: Event::new()?,
        stopping: AtomicBool::new(false),
        invalidated: AtomicU32::new(0),
        packets: AtomicU64::new(0),
        delivered_frames: AtomicU64::new(0),
        silent_frames: AtomicU64::new(0),
        discontinuities: AtomicU64::new(0),
        gap_frames: AtomicU64::new(0),
        timestamp_errors: AtomicU64::new(0),
    });
    let (started, ready) = mpsc::sync_channel(1);
    let query = query.to_owned();
    let worker = {
        let state = state.clone();
        thread::Builder::new()
            .name("Windows audio capture".into())
            .spawn(move || {
                let mut running = false;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _apartment = Apartment::new()?;
                        thread_start();
                        let enumerator = enumerator()?;
                        let endpoint = selected_endpoint(&enumerator, flow, &query)?;
                        let _notifications = Notifications::new(
                            enumerator,
                            &endpoint,
                            flow,
                            query.eq_ignore_ascii_case("default"),
                            state.clone(),
                        )?;
                        let capture = Session::new(&endpoint, channels.as_deref(), &state)?;
                        state.check_device()?;
                        unsafe { capture.client.Start() }.context("WASAPI Start")?;
                        if started
                            .send(Ok((endpoint.device.clone(), capture.pick.len() as u32)))
                            .is_err()
                        {
                            return Ok(());
                        }
                        running = true;
                        capture.run(&state, &sink)
                    }));
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("Windows audio capture: {error:#}"),
                    Err(_) => "Windows audio capture worker panicked".into(),
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
    match ready.recv() {
        Ok(Ok((device, channels))) => Ok((
            AudioCapture {
                state,
                worker: Some(worker),
                device,
            },
            channels,
        )),
        result => {
            state.stopping.store(true, Ordering::Release);
            state.stop.signal();
            let _ = worker.join();
            match result {
                Ok(Err(error)) => bail!("{error}"),
                _ => bail!("Windows audio capture worker exited before startup"),
            }
        }
    }
}

struct ComAllocation<T>(*mut T);

impl<T> Drop for ComAllocation<T> {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?)
}

fn device_id(device: &IMMDevice) -> Result<String> {
    let id = ComAllocation(unsafe { device.GetId() }?.0);
    if id.0.is_null() {
        bail!("audio endpoint returned a null id");
    }
    Ok(unsafe { PWSTR(id.0).to_string() }?)
}

fn default_id(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Result<Option<String>> {
    match unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) } {
        Ok(device) => Ok(Some(device_id(&device)?)),
        Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(None),
        Err(error) => Err(error).context("get default eConsole audio endpoint"),
    }
}

// Default markers are informational for explicit device selection/listing.
// Failure here must not prevent capture of a different, healthy endpoint.
fn reported_default_id(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Option<String> {
    match default_id(enumerator, flow) {
        Ok(id) => id,
        Err(error) => {
            eprintln!(
                "Windows audio devices: cannot mark default for flow {}: {error:#}",
                flow.0
            );
            None
        }
    }
}

struct EndpointIdentity {
    raw: IMMDevice,
    id: String,
    name: String,
}

impl EndpointIdentity {
    fn new(raw: IMMDevice, flow: EDataFlow) -> Result<Self> {
        let id = device_id(&raw)?;
        let endpoint: IMMEndpoint = raw.cast()?;
        if unsafe { endpoint.GetDataFlow() }? != flow {
            bail!(
                "audio.device: {id:?} is not a {} endpoint",
                if flow == eCapture {
                    "capture"
                } else {
                    "render"
                }
            );
        }
        if unsafe { raw.GetState() }? != DEVICE_STATE_ACTIVE {
            bail!("audio.device: {id:?} is not active");
        }
        let properties = unsafe { raw.OpenPropertyStore(STGM_READ) }
            .with_context(|| format!("audio endpoint {id:?}: open identity properties"))?;
        // windows-rs PROPVARIANT and BSTR both clear themselves on drop.
        let name = unsafe { properties.GetValue(&PKEY_Device_FriendlyName) }
            .with_context(|| format!("audio endpoint {id:?}: read friendly name"))?;
        let name = BSTR::try_from(&name)
            .with_context(|| format!("audio endpoint {id:?}: decode friendly name"))?
            .to_string();
        Ok(Self { raw, id, name })
    }
}

/// Identity discovery never activates an audio client. An unrelated endpoint
/// that disappears or has unreadable properties is diagnosed and skipped.
fn endpoint_identities(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> Result<Vec<EndpointIdentity>> {
    let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }?;
    let mut endpoints = Vec::new();
    for index in 0..unsafe { collection.GetCount() }? {
        let identity =
            (|| -> Result<_> { EndpointIdentity::new(unsafe { collection.Item(index) }?, flow) })();
        match identity {
            Ok(identity) => endpoints.push(identity),
            Err(error) => eprintln!(
                "Windows audio devices: skipping flow {} endpoint {index}: {error:#}",
                flow.0
            ),
        }
    }
    Ok(endpoints)
}

fn resolve(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    query: &str,
) -> Result<EndpointIdentity> {
    if query.eq_ignore_ascii_case("default") {
        let raw = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
            .context("audio.device: cannot resolve default eConsole endpoint")?;
        return EndpointIdentity::new(raw, flow);
    }
    if query.contains('\0') {
        bail!("audio.device: endpoint id/name must not contain NUL");
    }
    let id: Vec<u16> = query.encode_utf16().chain(Some(0)).collect();
    match unsafe { enumerator.GetDevice(PCWSTR(id.as_ptr())) } {
        Ok(raw) => {
            let identity = EndpointIdentity::new(raw, flow)?;
            if identity.id != query {
                bail!(
                    "audio.device: {query:?} is not an exact endpoint id; valid id: {:?}",
                    identity.id
                );
            }
            return Ok(identity);
        }
        // GetDevice rejects human-readable names as malformed IDs. They are
        // still valid queries for identity-only matching below.
        Err(error)
            if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
                || error.code() == E_INVALIDARG => {}
        Err(error) => {
            return Err(error).with_context(|| format!("audio.device: resolve {query:?}"));
        }
    }
    // Only a name query needs enumeration. Resolve it before any mix query;
    // once selected, failures propagate without trying a different endpoint.
    let mut endpoints = endpoint_identities(enumerator, flow)?;
    let lower = query.to_lowercase();
    let exact_name = endpoints
        .iter()
        .any(|endpoint| endpoint.name.to_lowercase() == lower);
    let matches: Vec<usize> = endpoints
        .iter()
        .enumerate()
        .filter(|(_, endpoint)| {
            if exact_name {
                endpoint.name.to_lowercase() == lower
            } else {
                !query.is_empty() && endpoint.name.to_lowercase().contains(&lower)
            }
        })
        .map(|(index, _)| index)
        .collect();
    if matches.len() == 1 {
        return Ok(endpoints.remove(matches[0]));
    }
    let choices = endpoints
        .iter()
        .map(|endpoint| format!("{} ({})", endpoint.name, endpoint.id))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "audio.device: {query:?} matched {} active endpoints; valid choices: {}",
        matches.len(),
        if choices.is_empty() {
            "(none)"
        } else {
            &choices
        }
    );
}

struct Endpoint {
    raw: IMMDevice,
    client: IAudioClient,
    device: AudioDevice,
    mask: u32,
}

fn selected_endpoint(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    query: &str,
) -> Result<Endpoint> {
    let identity = resolve(enumerator, flow, query)?;
    let is_default = query.eq_ignore_ascii_case("default")
        || reported_default_id(enumerator, flow).as_deref() == Some(identity.id.as_str());
    open_endpoint(identity, flow, is_default)
}

fn open_endpoint(identity: EndpointIdentity, flow: EDataFlow, default: bool) -> Result<Endpoint> {
    let EndpointIdentity { raw, id, name } = identity;
    let client: IAudioClient = unsafe { raw.Activate(CLSCTX_ALL, None) }.with_context(|| {
        format!("audio endpoint {name:?} ({id}): activate for mix-format metadata")
    })?;
    let mix = ComAllocation(
        unsafe { client.GetMixFormat() }
            .with_context(|| format!("audio endpoint {name:?} ({id}): query mix format"))?,
    );
    if mix.0.is_null() {
        bail!("audio endpoint {name:?}: null mix format");
    }
    // WAVEFORMAT* is packed. Copy fields before formatting or borrowing.
    let format = unsafe { mix.0.read_unaligned() };
    let channels = u32::from(format.nChannels);
    let sample_rate = format.nSamplesPerSec;
    if channels == 0 || sample_rate == 0 {
        bail!("audio endpoint {name:?}: invalid mix {channels} channels/{sample_rate} Hz");
    }
    let mask = if u32::from(format.wFormatTag) == WAVE_FORMAT_EXTENSIBLE {
        if format.cbSize < 22 {
            bail!("audio endpoint {name:?}: truncated extensible mix format");
        }
        unsafe { mix.0.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() }.dwChannelMask
    } else {
        match channels {
            1 => 4,
            2 => 3,
            _ => 0,
        }
    };
    Ok(Endpoint {
        raw,
        client,
        mask,
        device: AudioDevice {
            id,
            name,
            input: flow == eCapture,
            default,
            channels,
            sample_rate,
        },
    })
}

fn channel_selection(device: &AudioDevice, channels: Option<&[u32]>) -> Result<(u32, Vec<usize>)> {
    validate_channel_count(channels)?;
    let count = if device.input { device.channels } else { 2 };
    if !(1..=MAX_CHANNELS).contains(&count) {
        bail!(
            "audio endpoint {:?}: {count} capture channels; supported range: 1..={MAX_CHANNELS}",
            device.name
        );
    }
    if channels.is_some_and(|channels| {
        channels
            .iter()
            .any(|channel| !(1..=count).contains(channel))
            || (channels.len() == 2 && channels[0] == channels[1])
    }) {
        bail!(
            "microphone.channels: {channels:?}; choose one or two distinct channels in 1..={count} for {:?}",
            device.name
        );
    }
    let pick = channels.map_or_else(
        || (0..count.min(2) as usize).collect(),
        |channels| {
            channels
                .iter()
                .map(|&channel| (channel - 1) as usize)
                .collect()
        },
    );
    Ok((count, pick))
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    state: Arc<State>,
}

impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _: u32,
        _: *const f32,
        _: u32,
        _: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> windows::core::Result<()> {
        // Inactive is legitimate for idle loopback, not a lost session.
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> windows::core::Result<()> {
        // Format changes/service shutdown need not change endpoint identity
        // or produce another buffer event. Wake an otherwise infinite wait.
        self.state.invalidate(3);
        Ok(())
    }
}

struct SessionNotifications {
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
}

impl Drop for SessionNotifications {
    fn drop(&mut self) {
        let _ = unsafe {
            self.control
                .UnregisterAudioSessionNotification(&self.events)
        };
    }
}

struct Session {
    // Unregister before releasing the client; callbacks retain only State.
    _notifications: SessionNotifications,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    channels: usize,
    pick: Vec<usize>,
    frames: u32,
    native_rate: u32,
}

impl Session {
    fn new(endpoint: &Endpoint, channels: Option<&[u32]>, state: &Arc<State>) -> Result<Self> {
        let input = endpoint.device.input;
        let (count, pick) = channel_selection(&endpoint.device, channels)?;
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: count as u16,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * count * 4,
                nBlockAlign: (count * 4) as u16,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: if input { endpoint.mask } else { 3 },
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        // Reuse the selected client's fresh mix-format query, not a listing
        // snapshot or a second activation that could observe a different mix.
        let client = endpoint.client.clone();
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | if input {
                0
            } else {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            };
        // Initialize with conversion is authoritative. Do not mistake an
        // IsFormatSupported S_FALSE/closest native format for our PCM format.
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &format.Format, None) }
            .with_context(|| format!("WASAPI Initialize {:?}: shared {count}-channel 48000 Hz float32 with OS conversion{}", endpoint.device.name, if input { "" } else { " and loopback" }))?;
        let frames = unsafe { client.GetBufferSize() }?;
        if !(1..=MAX_FRAMES).contains(&frames) {
            bail!("WASAPI buffer size {frames}; supported frames: 1..={MAX_FRAMES}");
        }
        unsafe { client.SetEventHandle(state.data.handle()) }.context("WASAPI SetEventHandle")?;
        let capture = unsafe { client.GetService() }.context("WASAPI capture service")?;
        let notifications = SessionNotifications {
            control: unsafe { client.GetService() }.context("WASAPI session control")?,
            events: SessionEvents {
                state: state.clone(),
            }
            .into(),
        };
        unsafe {
            notifications
                .control
                .RegisterAudioSessionNotification(&notifications.events)
        }
        .context("WASAPI session-disconnection notifications")?;
        Ok(Self {
            _notifications: notifications,
            client,
            capture,
            channels: count as usize,
            pick,
            frames,
            native_rate: endpoint.device.sample_rate,
        })
    }

    fn run(&self, state: &State, sink: &AudioSink) -> Result<()> {
        let mut samples = Vec::with_capacity(self.frames as usize * self.pick.len());
        let mut clock = PacketClock::new(self.native_rate, self.frames);
        loop {
            if state.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            state.check_device()?;
            // Windows 10 1703+ loopback and capture signal the event for data.
            // Endpoint changes signal `data` too, and Drop signals `stop`;
            // an idle render endpoint needs no periodic GetNextPacketSize.
            let wait = unsafe {
                WaitForMultipleObjects(&[state.stop.handle(), state.data.handle()], false, INFINITE)
            };
            if wait == WAIT_OBJECT_0 {
                return Ok(());
            }
            if wait == WAIT_FAILED {
                return Err(windows::core::Error::from_thread()).context("WASAPI wait");
            }
            if wait.0 != WAIT_OBJECT_0.0 + 1 {
                bail!("unexpected WASAPI wait result: {}", wait.0);
            }
            loop {
                if state.stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
                state.check_device()?;
                let next = unsafe { self.capture.GetNextPacketSize() }
                    .context("WASAPI GetNextPacketSize")?;
                if next == 0 {
                    break;
                }
                if next > self.frames {
                    bail!("WASAPI packet {next} exceeds buffer {}", self.frames);
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags, mut position, mut qpc) = (0, 0, 0, 0);
                unsafe {
                    self.capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        Some(&mut position),
                        Some(&mut qpc),
                    )
                }
                .context("WASAPI GetBuffer")?;
                if frames == 0 {
                    bail!("WASAPI returned an empty packet after announcing {next} frames");
                }
                let lease = Packet {
                    capture: &self.capture,
                    frames,
                    held: true,
                };
                state.packets.fetch_add(1, Ordering::Relaxed);
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    state.discontinuities.fetch_add(1, Ordering::Relaxed);
                }
                if frames > self.frames {
                    bail!(
                        "WASAPI returned {frames} frames for a {}-frame buffer",
                        self.frames
                    );
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                if silent {
                    state
                        .silent_frames
                        .fetch_add(u64::from(frames), Ordering::Relaxed);
                }
                let (time, gap) = clock
                    .timestamp(frames, position, qpc, flags)
                    .inspect_err(|_| {
                        state.timestamp_errors.fetch_add(1, Ordering::Relaxed);
                    })
                    .with_context(|| {
                        format!(
                            "WASAPI invalid/uncertain timing: flags={flags:#x}, native position={position}, native rate={}, QPC100ns={qpc}",
                            self.native_rate
                        )
                    })?;
                state.gap_frames.fetch_add(gap, Ordering::Relaxed);
                samples.clear();
                if silent {
                    samples.resize(frames as usize * self.pick.len(), 0.0);
                } else {
                    let length = (frames as usize)
                        .checked_mul(self.channels)
                        .context("WASAPI sample length overflow")?;
                    let bytes = length
                        .checked_mul(size_of::<f32>())
                        .context("WASAPI byte length overflow")?;
                    if data.is_null()
                        || !(data as usize).is_multiple_of(align_of::<f32>())
                        || bytes > isize::MAX as usize
                        || (data as usize).checked_add(bytes).is_none()
                    {
                        bail!("WASAPI returned an invalid float32 packet pointer/length");
                    }
                    // WASAPI owns this checked, bounded region until ReleaseBuffer.
                    let source = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), length) };
                    for frame in source.chunks_exact(self.channels) {
                        if frame.iter().any(|sample| !sample.is_finite()) {
                            bail!("WASAPI returned non-finite float32 samples");
                        }
                        for &channel in &self.pick {
                            samples.push(frame[channel]);
                        }
                    }
                }
                // Never hold the native packet while user code runs or stalls.
                lease.release()?;
                if state.stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
                state.check_device()?;
                (sink.samples)(&samples, time);
                state
                    .delivered_frames
                    .fetch_add(u64::from(frames), Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

#[derive(Clone, Copy)]
struct ClockPacket {
    position: u64,
    qpc: u64,
    nanos: i64,
    frames: u32,
}

/// AUTOCONVERTPCM can leave device positions in the endpoint's native rate.
/// Keep that domain separate from produced 48 kHz frames. The first packet
/// anchors position only; every PTS still comes from the current packet's QPC.
struct PacketClock {
    native_rate: u32,
    buffer_frames: u32,
    anchor: Option<u64>,
    output_frames: u64,
    expected_debt: Option<i128>,
    previous: Option<ClockPacket>,
}

impl PacketClock {
    fn new(native_rate: u32, buffer_frames: u32) -> Self {
        Self {
            native_rate,
            buffer_frames,
            anchor: None,
            output_frames: 0,
            expected_debt: None,
            previous: None,
        }
    }

    fn timestamp(
        &mut self,
        frames: u32,
        position: u64,
        qpc: u64,
        flags: u32,
    ) -> Result<(Time, u64)> {
        if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 || qpc == 0 {
            bail!("WASAPI reported an invalid packet timestamp; lost frames are unknown");
        }
        if self.previous.is_some_and(|previous| qpc <= previous.qpc) {
            bail!("WASAPI QPC did not advance");
        }
        let raw_nanos = qpc
            .checked_mul(100)
            .and_then(|value| i64::try_from(value).ok())
            .context("WASAPI QPC timestamp overflow")?;
        let output_frames = self
            .output_frames
            .checked_add(u64::from(frames))
            .context("WASAPI cumulative output-frame overflow")?;
        let anchor = self.anchor.unwrap_or(position);
        let mut expected_debt = self.expected_debt;
        let (nanos, gap) = if self.native_rate == SAMPLE_RATE {
            // Only here do native positions and output counts have equal units.
            position
                .checked_add(u64::from(frames))
                .context("WASAPI native-frame position overflow")?;
            let gap = if let Some(previous) = self.previous {
                let end = previous
                    .position
                    .checked_add(u64::from(previous.frames))
                    .context("WASAPI native-frame position overflow")?;
                position
                    .checked_sub(end)
                    .context("WASAPI native-frame position moved backwards or overlapped")?
            } else {
                0
            };
            (raw_nanos, gap)
        } else {
            if self.native_rate == 0 {
                bail!("WASAPI native sample rate is zero");
            }
            if let Some(previous) = self.previous {
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    bail!(
                        "WASAPI resampled discontinuity after startup; exact lost converted frames are unknown"
                    );
                }
                if position <= previous.position {
                    bail!("WASAPI native-frame position did not advance during resampling");
                }
            }
            let native_elapsed = position
                .checked_sub(anchor)
                .context("WASAPI native-frame position precedes its anchor")?;
            // u64 native frames × 48000 fits u128. Floor once, cumulatively,
            // rather than rounding each packet and accumulating rate error.
            let elapsed_output =
                u128::from(native_elapsed) * u128::from(SAMPLE_RATE) / u128::from(self.native_rate);
            let debt = i128::try_from(elapsed_output)
                .context("WASAPI converted-frame position overflow")?
                - i128::from(self.output_frames);
            if let Some(expected) = expected_debt {
                if (debt - expected).abs() > 2 {
                    bail!(
                        "WASAPI resampler debt changed from {expected} to {debt} output frames (tolerance 2); exact lost converted frames are unknown"
                    );
                }
            } else if self.previous.is_some() {
                // The second packet exposes startup lookahead (e.g. 63 frames
                // for 24 kHz -> 48 kHz). Freeze it; do not learn around loss.
                if debt.abs() > i128::from(self.buffer_frames) {
                    bail!(
                        "WASAPI initial resampler debt {debt} exceeds the {}-frame buffer",
                        self.buffer_frames
                    );
                }
                expected_debt = Some(debt);
            }
            // Use THIS packet's QPC, not the first QPC plus a sample counter:
            // subtracting bounded SRC debt retains the device clock's drift.
            let nanos = i128::from(raw_nanos) - debt * 1_000_000_000 / i128::from(SAMPLE_RATE);
            let nanos = i64::try_from(nanos).context("WASAPI corrected timestamp overflow")?;
            if nanos < 0 {
                bail!("WASAPI corrected timestamp precedes the QPC origin");
            }
            (nanos, 0)
        };
        if self
            .previous
            .is_some_and(|previous| nanos <= previous.nanos)
        {
            bail!("WASAPI corrected packet timestamp did not advance");
        }
        self.anchor = Some(anchor);
        self.output_frames = output_frames;
        self.expected_debt = expected_debt;
        self.previous = Some(ClockPacket {
            position,
            qpc,
            nanos,
            frames,
        });
        Ok((Time::from_nanos(nanos), gap))
    }
}

struct Packet<'a> {
    capture: &'a IAudioCaptureClient,
    frames: u32,
    held: bool,
}

impl Packet<'_> {
    fn release(mut self) -> Result<()> {
        self.held = false;
        unsafe { self.capture.ReleaseBuffer(self.frames) }.context("WASAPI ReleaseBuffer")
    }
}

impl Drop for Packet<'_> {
    fn drop(&mut self) {
        if self.held {
            let _ = unsafe { self.capture.ReleaseBuffer(self.frames) };
        }
    }
}

#[implement(IMMNotificationClient)]
struct Notification {
    state: Arc<State>,
    id: Vec<u16>,
    flow: EDataFlow,
    default: bool,
}

impl Notification {
    fn selected(&self, id: &PCWSTR) -> bool {
        !id.is_null() && unsafe { id.as_wide() } == self.id
    }
}

impl IMMNotificationClient_Impl for Notification_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        if self.selected(id) && state != DEVICE_STATE_ACTIVE {
            self.state.invalidate(1);
        }
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        if self.selected(id) {
            self.state.invalidate(1);
        }
        Ok(())
    }
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if self.default && flow == self.flow && role == eConsole && !self.selected(id) {
            self.state.invalidate(2);
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

struct Notifications {
    enumerator: IMMDeviceEnumerator,
    callback: IMMNotificationClient,
}

impl Notifications {
    fn new(
        enumerator: IMMDeviceEnumerator,
        endpoint: &Endpoint,
        flow: EDataFlow,
        default: bool,
        state: Arc<State>,
    ) -> Result<Self> {
        // No COM pointers in callbacks, and no cycle back to this registration.
        let callback = Notification {
            state,
            id: endpoint.device.id.encode_utf16().collect(),
            flow,
            default,
        }
        .into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&callback) }
            .context("register audio endpoint notifications")?;
        let registration = Self {
            enumerator,
            callback,
        };
        // Close the selection-to-registration race before starting a stream.
        if unsafe { endpoint.raw.GetState() }? != DEVICE_STATE_ACTIVE {
            bail!("selected audio endpoint became inactive during startup");
        }
        if default
            && default_id(&registration.enumerator, flow)?.as_deref()
                != Some(endpoint.device.id.as_str())
        {
            bail!(
                "default eConsole audio endpoint changed during startup; reopen capture explicitly"
            );
        }
        Ok(registration)
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.callback)
        };
    }
}
