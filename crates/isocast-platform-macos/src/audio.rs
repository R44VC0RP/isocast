//! Audio capture: microphones through AVFoundation, and what the computer
//! or an app plays through Core Audio process taps (`tap.rs`, macOS 14.2+)
//! or, before 14.2, ScreenCaptureKit.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use isocast_engine::{AudioSink, SAMPLE_RATE, SamplesCallback};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVCaptureAudioDataOutput, AVCaptureAudioDataOutputSampleBufferDelegate, AVCaptureConnection,
    AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession,
};
use objc2_avf_audio::{
    AVFormatIDKey, AVLinearPCMBitDepthKey, AVLinearPCMIsFloatKey, AVLinearPCMIsNonInterleaved,
    AVNumberOfChannelsKey, AVSampleRateKey,
};
use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput,
};
use objc2_core_audio_types::{
    AudioBufferList, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMClock, CMSampleBuffer,
    CMTime, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};

use crate::camera::{host_time, watch};
use crate::devices::{find_display, find_microphone, shareable_content};
use crate::tap::{self, Backend, TapCapture, Target};
use crate::writer::catch;

/// Channels recorded from a microphone by default at most: a 1- or
/// 2-input device's own signal, or a larger interface's inputs 1 + 2
/// (unless the source's `channels` picks inputs).
const MAX_CHANNELS: u32 = 2;
/// How long starting or stopping a stream may take.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// A running microphone capture. Dropping it stops the session.
pub struct MicrophoneCapture {
    session: Retained<AVCaptureSession>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    _watch: Option<ChannelWatch>,
    _delegate: Retained<AudioDelegate>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: as for cameras; the session may be stopped from any thread.
unsafe impl Send for MicrophoneCapture {}

impl Drop for MicrophoneCapture {
    fn drop(&mut self) {
        crate::camera::unwatch(&self.observers);
        unsafe { self.session.stopRunning() };
    }
}

/// Start capturing the microphone `query` names ("default" for the system
/// default input): the input channels `channels` picks (from 1), or by
/// default its inputs 1 + 2. Returns the capture and the channels delivered.
pub fn capture_microphone(
    query: &str,
    channels: Option<&[u32]>,
    sink: AudioSink,
) -> Result<(MicrophoneCapture, u32)> {
    let device = find_microphone(query)?;
    let name = unsafe { device.localizedName() }.to_string();
    let format = microphone_format(&device, channels)?;
    let channels = format.channels();

    let input =
        unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.map_err(|err| {
            anyhow!(
                "could not open {name:?}: {} (microphone permission: System Settings → \
                 Privacy & Security → Microphone)",
                err.localizedDescription()
            )
        })?;
    let session = unsafe { AVCaptureSession::new() };
    let output = unsafe { AVCaptureAudioDataOutput::new() };
    let delegate = AudioDelegate::new(sink, format.request, format.pick, format.default, &name);
    let queue = DispatchQueue::new("isocast.microphone-capture", None);
    // Ask AVFoundation for the engine's format, resampled if needed.
    let settings = pcm_settings(format.request);
    unsafe {
        output.setAudioSettings(Some(&settings));
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
    }
    catch(|| unsafe {
        session.beginConfiguration();
        let added = session.canAddInput(&input) && session.canAddOutput(&output);
        if added {
            session.addInput(&input);
            session.addOutput(&output);
        }
        session.commitConfiguration();
        added
    })?
    .then_some(())
    .with_context(|| format!("{name:?} can't be added to a capture session"))?;

    *delegate.ivars().clock.lock().unwrap() = unsafe { session.synchronizationClock() };
    let stopped = delegate.ivars().stopped.clone();
    let observers = watch(&session, &device, move |reason| stopped(reason));
    let channel_watch = format
        .core_audio
        .zip(delegate.ivars().check.as_ref())
        .map(|(device, check)| ChannelWatch::new(device, check));
    unsafe { session.startRunning() };
    if !unsafe { session.isRunning() } {
        bail!("{name:?} did not start");
    }
    Ok((
        MicrophoneCapture {
            session,
            observers,
            _watch: channel_watch,
            _delegate: delegate,
            _queue: queue,
        },
        channels,
    ))
}

/// What a microphone capture asks AVFoundation for, and keeps.
pub(crate) struct MicrophoneFormat {
    /// The device's first `request` channels are asked for.
    request: u32,
    /// Which of those are delivered (from 0), in order; `None` delivers
    /// all of them.
    pick: Option<Vec<usize>>,
    /// The Core Audio device, watched for format changes while picked
    /// channels are recorded.
    core_audio: Option<AudioObjectID>,
    /// No `channels` given: a larger interface's inputs 1 + 2, which fall
    /// back to input 1 instead of stopping if the device shrinks.
    default: bool,
}

impl MicrophoneFormat {
    /// The channels delivered.
    pub(crate) fn channels(&self) -> u32 {
        self.pick
            .as_ref()
            .map_or(self.request, |pick| pick.len() as u32)
    }
}

/// How to capture `channels` (from 1; by default a 1- or 2-input
/// device's own signal, a larger one's inputs 1 + 2) of a microphone. The
/// error names the channel and lists the device's.
pub(crate) fn microphone_format(
    device: &AVCaptureDevice,
    channels: Option<&[u32]>,
) -> Result<MicrophoneFormat> {
    let most = device_channels(device);
    // Inputs are counted in the format the device runs now: an ADAT
    // interface at 96 kHz has fewer inputs than its formats list, and
    // AVFoundation's formats are cached per process (#99).
    let core_audio = crate::output::device_with_uid(&unsafe { device.uniqueID() }.to_string());
    let available = core_audio
        .map(crate::output::input_channels)
        .filter(|channels| *channels > 0)
        .unwrap_or(most);
    let Some(channels) = channels else {
        if available <= MAX_CHANNELS {
            // One or two inputs: the device's own signal, as before.
            return Ok(MicrophoneFormat {
                request: most.clamp(1, MAX_CHANNELS),
                pick: None,
                core_audio: None,
                default: false,
            });
        }
        // More: inputs 1 + 2 as stereo, read like picked inputs, since
        // AVFoundation asked for two mixes every input down (#99, Ryan's
        // call: an interface's first pair, not a mix of all of them).
        return Ok(MicrophoneFormat {
            request: available,
            pick: Some(vec![0, 1]),
            core_audio,
            default: true,
        });
    };
    if channels.is_empty() || channels.len() > 2 {
        bail!("channels: give one channel (mono) or two (stereo)");
    }
    // Picked inputs are checked against the current format: a channel the
    // device lacks would be AVFoundation's upmix of the others.
    if let Some((i, channel)) = channels
        .iter()
        .enumerate()
        .find(|(_, c)| **c == 0 || **c > available)
    {
        let uid = unsafe { device.uniqueID() }.to_string();
        let names = crate::output::input_channel_names(&uid);
        let now = if available < most {
            format!(" in its current format (up to {most} in others)")
        } else {
            String::new()
        };
        let valid: Vec<String> = (1..=available)
            .map(
                |c| match names.get(c as usize - 1).filter(|n| !n.is_empty()) {
                    Some(name) => format!("{c} ({name})"),
                    None => c.to_string(),
                },
            )
            .collect();
        bail!(
            "channels[{i}]: {:?} has no input channel {channel}{now}; valid: {}",
            unsafe { device.localizedName() }.to_string(),
            valid.join(", ")
        );
    }
    // Asked for fewer channels than the device has, AVFoundation mixes
    // them down instead of dropping the rest (BlackHole 16ch asked for 2,
    // 3 or 4: mixes of all 16, or silence), so it is asked for all of
    // them, unconverted, and only the picked ones go on.
    Ok(MicrophoneFormat {
        request: available,
        pick: Some(channels.iter().map(|c| *c as usize - 1).collect()),
        core_audio,
        default: false,
    })
}

/// `samples`, interleaved with `channels` channels, as only the channels
/// `pick` names (from 0), in its order.
fn pick_channels(samples: &[f32], channels: usize, pick: &[usize]) -> Vec<f32> {
    let mut out = Vec::with_capacity(samples.len() / channels.max(1) * pick.len());
    for frame in samples.chunks_exact(channels.max(1)) {
        out.extend(pick.iter().map(|&c| frame[c]));
    }
    out
}

/// The most input channels any of the device's formats has.
fn device_channels(device: &AVCaptureDevice) -> u32 {
    unsafe { device.formats() }
        .iter()
        .filter_map(|format| {
            let description = unsafe { format.formatDescription() };
            let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description) };
            unsafe { asbd.as_ref() }.map(|asbd| asbd.mChannelsPerFrame)
        })
        .max()
        .unwrap_or(1)
}

pub(crate) fn pcm_settings(channels: u32) -> Retained<NSDictionary<NSString, AnyObject>> {
    let format = NSNumber::new_u32(kAudioFormatLinearPCM);
    let rate = NSNumber::new_f64(SAMPLE_RATE as f64);
    let channels = NSNumber::new_u32(channels);
    let bits = NSNumber::new_u32(32);
    let yes = NSNumber::new_bool(true);
    let no = NSNumber::new_bool(false);
    unsafe {
        NSDictionary::from_slices(
            &[
                AVFormatIDKey,
                AVSampleRateKey,
                AVNumberOfChannelsKey,
                AVLinearPCMBitDepthKey,
                AVLinearPCMIsFloatKey,
                AVLinearPCMIsNonInterleaved,
            ]
            .map(|key| key.expect("AVFoundation audio settings key")),
            &[&*format, &*rate, &*channels, &*bits, &*yes, &*no],
        )
    }
}

pub struct AudioIvars {
    samples: SamplesCallback,
    stopped: Arc<dyn Fn(String) + Send + Sync>,
    channels: u32,
    /// The channels delivered (from 0) of `channels`; all if `None`.
    pick: Option<Vec<usize>>,
    /// Stops a capture of picked channels once one of them is gone.
    check: Option<Arc<ChannelCheck>>,
    /// The session's clock, which sample times are on; none for
    /// ScreenCaptureKit, which uses the host clock.
    clock: Mutex<Option<Retained<CMClock>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and AudioDelegate
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "IsocastAudioOutput"]
    #[ivars = AudioIvars]
    pub struct AudioDelegate;

    unsafe impl NSObjectProtocol for AudioDelegate {}

    unsafe impl AVCaptureAudioDataOutputSampleBufferDelegate for AudioDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.deliver(sample);
        }
    }

    unsafe impl SCStreamOutput for AudioDelegate {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Audio {
                self.deliver(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            (self.ivars().stopped)(error.localizedDescription().to_string());
        }
    }
);

impl AudioDelegate {
    /// `name` names the device in errors.
    /// `default`: `pick` is a larger interface's default inputs 1 + 2.
    fn new(
        sink: AudioSink,
        channels: u32,
        pick: Option<Vec<usize>>,
        default: bool,
        name: &str,
    ) -> Retained<Self> {
        let stopped: Arc<dyn Fn(String) + Send + Sync> = Arc::from(sink.stopped);
        let check = pick.as_ref().map(|pick| {
            Arc::new(ChannelCheck {
                name: name.to_string(),
                opened: channels,
                seen: AtomicU32::new(channels),
                picked: pick.iter().map(|c| *c as u32 + 1).collect(),
                default,
                mono: AtomicBool::new(false),
                stopped: stopped.clone(),
                failed: AtomicBool::new(false),
            })
        });
        let this = Self::alloc().set_ivars(AudioIvars {
            samples: sink.samples,
            stopped,
            channels,
            pick,
            check,
            clock: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn deliver(&self, sample: &CMSampleBuffer) {
        let ivars = self.ivars();
        let mut mono = false;
        if let Some(check) = &ivars.check {
            let delivered = delivered_channels(sample).unwrap_or(ivars.channels);
            if check.default {
                mono = check.default_mono(delivered);
            } else if !check.check(delivered, "delivers") {
                return;
            }
        }
        match interleaved_f32(sample, ivars.channels) {
            Ok(Some(samples)) => {
                let samples = match &ivars.pick {
                    // The default's input 2 is gone: input 1 in both.
                    Some(_) if mono => pick_channels(&samples, ivars.channels as usize, &[0, 0]),
                    Some(pick) => pick_channels(&samples, ivars.channels as usize, pick),
                    None => samples,
                };
                let time = unsafe { sample.presentation_time_stamp() };
                let clock = ivars.clock.lock().unwrap();
                (ivars.samples)(&samples, host_time(time, clock.as_deref()));
            }
            Ok(None) => {}
            Err(err) => (ivars.stopped)(format!("{err:#}")),
        }
    }
}

/// The channels a sample buffer holds, if it says.
fn delivered_channels(sample: &CMSampleBuffer) -> Option<u32> {
    let description = unsafe { sample.format_description() }?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description).as_ref() }?;
    Some(asbd.mChannelsPerFrame)
}

/// Picked channels are read from a device opened with `opened` inputs.
/// If it then has (or delivers) fewer than a picked channel needs (a format
/// change, an ADAT interface switched to 96 kHz), the capture stops with
/// the reason, like a disconnected device, instead of recording silence or
/// another input. A change that keeps every picked input records on:
/// AVFoundation keeps delivering them (BlackHole in an aggregate going
/// 16 -> 18 or 18 -> 16 inputs kept its input 3 exact).
///
/// A larger interface's default inputs 1 + 2 (`default`) never stop: with
/// fewer than two inputs left, input 1 is recorded in both channels until
/// input 2 is back, and the diagnostics say so.
struct ChannelCheck {
    name: String,
    opened: u32,
    /// The count Core Audio last reported (for the log, and the default's
    /// fallback).
    seen: AtomicU32,
    /// From 1.
    picked: Vec<u32>,
    default: bool,
    /// The default records input 1 in both channels.
    mono: AtomicBool,
    stopped: Arc<dyn Fn(String) + Send + Sync>,
    failed: AtomicBool,
}

impl ChannelCheck {
    /// Whether the device, which `what` (has or delivers) `now` channels,
    /// still gives the picked ones; stops the capture (once) if not.
    fn check(&self, now: u32, what: &str) -> bool {
        if self.failed.load(Ordering::Acquire) {
            return false;
        }
        let Some((i, channel)) = self.picked.iter().enumerate().find(|(_, c)| **c > now) else {
            return true;
        };
        let reason = format!(
            "channels[{i}]: {:?} now {what} {now} input channels instead of {}, so input \
             channel {channel} is gone (its format changed while recording)",
            self.name, self.opened
        );
        if !self.failed.swap(true, Ordering::AcqRel) {
            isocast_diagnostics::diagnostic!("warn"; "microphone: {reason}");
            (self.stopped)(reason);
        }
        false
    }

    /// Whether the default inputs 1 + 2 must fall back to input 1 alone,
    /// with the device delivering `delivered` channels and having the
    /// count last seen; says so when that changes.
    fn default_mono(&self, delivered: u32) -> bool {
        let now = delivered.min(self.seen.load(Ordering::Acquire));
        let mono = now < 2;
        if self.mono.swap(mono, Ordering::AcqRel) != mono {
            if mono {
                isocast_diagnostics::diagnostic!("warn";
                    "microphone: {:?} now has {now} input channel(s) instead of {}, so the \
                     default inputs 1 + 2 record input 1 in both channels until input 2 is \
                     back", self.name, self.opened);
            } else {
                isocast_diagnostics::diagnostic!("info";
                    "microphone: {:?} has input 2 again ({now} input channels); the default \
                     records inputs 1 + 2", self.name);
            }
        }
        mono
    }

    /// The device has `now` input channels: stop if a picked one is gone,
    /// and log any other change.
    fn changed(&self, now: u32) {
        // 0 is what Core Audio reports mid-reconfiguration (and when the
        // property can't be read): unknown, as validation treats it, never
        // a reason to stop the source.
        if now == 0 {
            return;
        }
        let was = self.seen.swap(now, Ordering::AcqRel);
        if self.default {
            // Never stops; the next buffer falls back if input 2 is gone.
            if was != now {
                isocast_diagnostics::diagnostic!("info";
                    "microphone: {:?} now has {now} input channels instead of {was}; the \
                     default records inputs 1 + 2 (input 1 alone in both channels with fewer \
                     than two)", self.name);
            }
            return;
        }
        if self.check(now, "has") && was != now {
            isocast_diagnostics::diagnostic!("info";
                "microphone: {:?} now has {now} input channels instead of {was}; the picked \
                 inputs {:?} are still there, so it records on", self.name, self.picked);
        }
    }
}

/// Core Audio listeners that run a [`ChannelCheck`] when the device's
/// input format changes. Removed on drop; the context they were given (a
/// weak reference) is never freed, as a call may still be under way.
struct ChannelWatch {
    device: AudioObjectID,
    context: *mut Weak<ChannelCheck>,
}

// SAFETY: the context is a Weak (Send + Sync) that is never freed.
unsafe impl Send for ChannelWatch {}

/// What a [`ChannelWatch`] listens for: the input streams' layout, and the
/// sample rate (ADAT inputs halve at 88.2/96 kHz).
const CHANNEL_EVENTS: [(u32, u32); 2] = [
    (
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeInput,
    ),
    (
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    ),
];

impl ChannelWatch {
    fn new(device: AudioObjectID, check: &Arc<ChannelCheck>) -> Self {
        let this = ChannelWatch {
            device,
            context: Box::into_raw(Box::new(Arc::downgrade(check))),
        };
        for (selector, scope) in CHANNEL_EVENTS {
            let address = crate::output::address(selector, scope);
            // SAFETY: the context is never freed.
            let status = unsafe {
                AudioObjectAddPropertyListener(
                    device,
                    NonNull::from(&address),
                    Some(channels_changed),
                    this.context.cast(),
                )
            };
            if status != 0 {
                isocast_diagnostics::diagnostic!("warn";
                    "microphone: couldn't watch {:?} for input format changes (Core Audio \
                     status {status}); a lost input is still caught by the capture", check.name);
            }
        }
        // A change between opening and listening counts too.
        check.changed(crate::output::input_channels(device));
        this
    }
}

impl Drop for ChannelWatch {
    fn drop(&mut self) {
        for (selector, scope) in CHANNEL_EVENTS {
            let address = crate::output::address(selector, scope);
            unsafe {
                AudioObjectRemovePropertyListener(
                    self.device,
                    NonNull::from(&address),
                    Some(channels_changed),
                    self.context.cast(),
                )
            };
        }
    }
}

unsafe extern "C-unwind" fn channels_changed(
    device: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    context: *mut c_void,
) -> i32 {
    // SAFETY: the context is a ChannelWatch's Weak, never freed.
    let check = unsafe { &*context.cast::<Weak<ChannelCheck>>() };
    if let Some(check) = check.upgrade() {
        check.changed(crate::output::input_channels(device));
    }
    0
}

/// A sample buffer's PCM audio as interleaved f32 with `channels`
/// channels. Missing channels are filled from the first; extra ones are
/// dropped. `None` for an empty buffer.
pub(crate) fn interleaved_f32(sample: &CMSampleBuffer, channels: u32) -> Result<Option<Vec<f32>>> {
    let frames = unsafe { sample.num_samples() } as usize;
    if frames == 0 {
        return Ok(None);
    }
    let description = unsafe { sample.format_description() }.context("audio without a format")?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description).as_ref() }
        .context("audio without a stream description")?;
    if asbd.mSampleRate != SAMPLE_RATE as f64 {
        bail!(
            "audio arrived at {} Hz instead of {SAMPLE_RATE} Hz",
            asbd.mSampleRate
        );
    }
    let float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let bits = asbd.mBitsPerChannel;
    if asbd.mFormatID != kAudioFormatLinearPCM
        || !matches!((float, bits), (true, 32) | (false, 16 | 32))
    {
        bail!("unsupported audio format ({bits}-bit, float: {float})");
    }

    // Ask how big the buffer list is, then fill one that size (it has one
    // buffer per channel for planar audio).
    let mut size = 0;
    unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            &mut size,
            ptr::null_mut(),
            0,
            None,
            None,
            0,
            ptr::null_mut(),
        )
    };
    let mut storage = vec![0u64; size.div_ceil(8).max(1)];
    let list = storage.as_mut_ptr().cast::<AudioBufferList>();
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            ptr::null_mut(),
            list,
            size,
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block,
        )
    };
    if status != 0 {
        bail!("could not read audio (CoreMedia error {status})");
    }
    // Owns the memory the buffer list points into.
    let _block = NonNull::new(block).map(|b| unsafe { CFRetained::from_raw(b) });
    let list = unsafe { &*list };
    let buffers =
        unsafe { slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };

    let out_channels = channels as usize;
    let mut out = vec![0.0f32; frames * out_channels];
    let mut channel = 0;
    for buffer in buffers {
        let in_channels = buffer.mNumberChannels.max(1) as usize;
        let bytes = buffer.mDataByteSize as usize;
        let data = buffer.mData.cast::<u8>().cast_const();
        if data.is_null() {
            continue;
        }
        let read = |index: usize| -> f32 {
            unsafe {
                match (float, bits) {
                    (true, _) => data.cast::<f32>().add(index).read_unaligned(),
                    (false, 16) => data.cast::<i16>().add(index).read_unaligned() as f32 / 32768.0,
                    _ => data.cast::<i32>().add(index).read_unaligned() as f32 / 2_147_483_648.0,
                }
            }
        };
        let available = bytes / (bits as usize / 8) / in_channels;
        for c in 0..in_channels {
            if channel + c >= out_channels {
                break;
            }
            for frame in 0..frames.min(available) {
                out[frame * out_channels + channel + c] = read(frame * in_channels + c);
            }
        }
        channel += in_channels;
        if !planar {
            break;
        }
    }
    // A mono source recorded as more channels plays in all of them.
    for c in channel.max(1)..out_channels {
        for frame in 0..frames {
            out[frame * out_channels + c] = out[frame * out_channels];
        }
    }
    Ok(Some(out))
}

/// A running system or app audio capture. Dropping it stops it.
pub struct SystemAudioCapture {
    _stream: Option<StreamAudio>,
    _tap: Option<TapCapture>,
}

impl SystemAudioCapture {
    fn stream(stream: StreamAudio) -> Self {
        SystemAudioCapture {
            _stream: Some(stream),
            _tap: None,
        }
    }

    fn tap(tap: TapCapture) -> Self {
        SystemAudioCapture {
            _stream: None,
            _tap: Some(tap),
        }
    }
}

/// Audio from a ScreenCaptureKit stream. Dropping it stops the stream.
struct StreamAudio {
    stream: Retained<SCStream>,
    _delegate: Retained<AudioDelegate>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: as for display capture.
unsafe impl Send for StreamAudio {}

impl Drop for StreamAudio {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = rx.recv_timeout(STREAM_TIMEOUT);
    }
}

/// The Isocast app's bundle id (crates/isocast-ui/Info.plist).
const ISOCAST_APP: &str = "dev.isocast.app";

/// Start capturing what the computer plays, as stereo, leaving out the
/// apps `exclude` names. `exclude_own` leaves out what this process plays
/// (monitoring).
pub fn capture_system_audio(
    exclude: &[String],
    sink: AudioSink,
    exclude_own: bool,
) -> Result<(SystemAudioCapture, u32)> {
    // The Isocast app is always left out (#94), by its bundle id exactly
    // (never the fuzzy name matching of `exclude`): its editor plays takes
    // aloud, which must not land in a recording, stream or replay before
    // the app learns that one started (the app also goes silent then).
    let exclude_bundles = vec![ISOCAST_APP.to_string()];
    let backend = tap::backend();
    isocast_diagnostics::diagnostic!("info";
        "system audio: capturing through {}", backend.label());
    match backend {
        Backend::ProcessTap => {
            let target = Target::System {
                exclude: exclude.to_vec(),
                exclude_bundles,
                exclude_own,
            };
            let (tap, channels) = tap::capture(target, sink)?;
            Ok((SystemAudioCapture::tap(tap), channels))
        }
        Backend::ScreenCaptureKit => {
            let (stream, channels) =
                capture_played_audio(None, exclude, &exclude_bundles, sink, exclude_own)?;
            Ok((SystemAudioCapture::stream(stream), channels))
        }
    }
}

/// Start capturing what one app plays, as stereo.
pub fn capture_application_audio(app: &str, sink: AudioSink) -> Result<(SystemAudioCapture, u32)> {
    let backend = tap::backend();
    isocast_diagnostics::diagnostic!("info";
        "application audio: capturing {app:?} through {}", backend.label());
    match backend {
        Backend::ProcessTap => {
            let (tap, channels) = tap::capture(Target::App(app.to_string()), sink)?;
            Ok((SystemAudioCapture::tap(tap), channels))
        }
        Backend::ScreenCaptureKit => {
            let (stream, channels) = capture_played_audio(Some(app), &[], &[], sink, false)?;
            Ok((SystemAudioCapture::stream(stream), channels))
        }
    }
}

/// What the computer (or only `app`) plays, through ScreenCaptureKit,
/// leaving out the running apps `exclude` names and those whose bundle id
/// is one of `exclude_bundles` (exactly).
fn capture_played_audio(
    app: Option<&str>,
    exclude: &[String],
    exclude_bundles: &[String],
    sink: AudioSink,
    exclude_own: bool,
) -> Result<(StreamAudio, u32)> {
    const CHANNELS: u32 = 2;
    // ScreenCaptureKit captures audio as part of a display stream; the
    // video is kept tiny and slow, and dropped.
    let (id, _) = find_display("main")?;
    let content = shareable_content().context(
        "screen recording permission is needed for system audio: System Settings → Privacy \
         & Security → Screen & System Audio Recording",
    )?;
    let display = unsafe { content.displays() }
        .iter()
        .find(|d| unsafe { d.displayID() } == id)
        .context("ScreenCaptureKit can't see the main display")?;
    let filter = match app {
        None => {
            // Apps that aren't running have nothing to leave out.
            let excluded: Vec<_> = unsafe { content.applications() }
                .iter()
                .filter(|a| {
                    exclude.iter().any(|q| crate::capture::app_matches(a, q))
                        || tap::bundle_listed(
                            &unsafe { a.bundleIdentifier() }.to_string(),
                            exclude_bundles,
                        )
                })
                .collect();
            isocast_diagnostics::diagnostic!("info";
                "system audio: ScreenCaptureKit leaves out {} app(s): {}",
                excluded.len(),
                excluded
                    .iter()
                    .map(|a| format!(
                        "{} ({}, {})",
                        unsafe { a.processID() },
                        unsafe { a.bundleIdentifier() },
                        unsafe { a.applicationName() }
                    ))
                    .collect::<Vec<_>>()
                    .join(", "));
            unsafe {
                SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::from_retained_slice(&excluded),
                    &NSArray::new(),
                )
            }
        }
        // Only the app's sound is captured.
        Some(app) => {
            let apps = crate::capture::find_applications(&content, app)?;
            unsafe {
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &apps,
                    &NSArray::new(),
                )
            }
        }
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setCapturesAudio(true);
        config.setSampleRate(SAMPLE_RATE as isize);
        config.setChannelCount(CHANNELS as isize);
        // Left out only when the session monitors: excluding "this process"
        // also silences whatever else the same app launched (recording
        // in-process from a terminal); the daemon is its own app, so there
        // it leaves out only Isocast's monitoring.
        config.setExcludesCurrentProcessAudio(exclude_own);
        config.setWidth(2);
        config.setHeight(2);
        config.setMinimumFrameInterval(CMTime::new(1, 1));
    }
    let delegate = AudioDelegate::new(sink, CHANNELS, None, false, "");
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*delegate)),
        )
    };
    let queue = DispatchQueue::new("isocast.system-audio", None);
    // Screen frames go to the same object, which ignores them; without a
    // screen output ScreenCaptureKit logs every dropped frame.
    for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*delegate),
                kind,
                Some(&queue),
            )
        }
        .map_err(|err| {
            anyhow!(
                "could not attach to the stream: {}",
                err.localizedDescription()
            )
        })?;
    }
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let error = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(error);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match rx.recv_timeout(STREAM_TIMEOUT) {
        Ok(None) => {}
        Ok(Some(error)) => bail!("could not start capturing system audio: {error}"),
        Err(_) => bail!("system audio capture did not start in time"),
    }
    Ok((
        StreamAudio {
            stream,
            _delegate: delegate,
            _queue: queue,
        },
        CHANNELS,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_channels_in_order() {
        // Three frames of four channels: frame f, channel c is f * 10 + c.
        let samples: Vec<f32> = (0..3)
            .flat_map(|f| (0..4).map(move |c| (f * 10 + c) as f32))
            .collect();
        assert_eq!(pick_channels(&samples, 4, &[2]), [2.0, 12.0, 22.0]);
        assert_eq!(
            pick_channels(&samples, 4, &[3, 0]),
            [3.0, 0.0, 13.0, 10.0, 23.0, 20.0]
        );
    }

    #[test]
    fn stops_once_when_a_picked_channel_is_gone() {
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let check = ChannelCheck {
            name: "Interface".into(),
            opened: 18,
            seen: AtomicU32::new(18),
            picked: vec![3, 9],
            default: false,
            mono: AtomicBool::new(false),
            stopped: Arc::new({
                let reasons = reasons.clone();
                move |reason| reasons.lock().unwrap().push(reason)
            }),
            failed: AtomicBool::new(false),
        };
        // Fewer or more inputs, the picked ones still there: records on.
        check.changed(16);
        check.changed(18);
        assert!(check.check(9, "delivers"));
        assert!(reasons.lock().unwrap().is_empty());
        // Input 9 gone: stops, once, naming the field and the count.
        check.changed(8);
        assert!(!check.check(18, "delivers"));
        check.changed(2);
        assert_eq!(
            *reasons.lock().unwrap(),
            [
                "channels[1]: \"Interface\" now has 8 input channels instead of 18, so input \
                 channel 9 is gone (its format changed while recording)"
            ]
        );
    }

    #[test]
    fn default_inputs_fall_back_instead_of_stopping() {
        let reasons = Arc::new(Mutex::new(Vec::new()));
        let check = ChannelCheck {
            name: "Interface".into(),
            opened: 18,
            seen: AtomicU32::new(18),
            picked: vec![1, 2],
            default: true,
            mono: AtomicBool::new(false),
            stopped: Arc::new({
                let reasons = reasons.clone();
                move |reason| reasons.lock().unwrap().push(reason)
            }),
            failed: AtomicBool::new(false),
        };
        assert!(!check.default_mono(18));
        // Down to two inputs: still inputs 1 + 2.
        check.changed(2);
        assert!(!check.default_mono(18));
        // One input (or one delivered): input 1 in both, never stopped.
        check.changed(1);
        assert!(check.default_mono(18));
        check.changed(18);
        assert!(check.default_mono(1));
        assert!(!check.default_mono(18));
        assert!(reasons.lock().unwrap().is_empty());
    }
}
