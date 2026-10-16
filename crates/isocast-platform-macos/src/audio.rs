//! Audio capture: microphones through AVFoundation, and what the computer
//! or an app plays through Core Audio process taps (`tap.rs`, macOS 14.2+)
//! or, before 14.2, ScreenCaptureKit.

use std::ptr::{self, NonNull};
use std::slice;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
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

/// Channels recorded from a microphone at most. Multi-channel interfaces
/// are recorded as their first two inputs for now.
const MAX_CHANNELS: u32 = 2;
/// How long starting or stopping a stream may take.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// A running microphone capture. Dropping it stops the session.
pub struct MicrophoneCapture {
    session: Retained<AVCaptureSession>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
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
/// default input). Returns the capture and its channel count.
pub fn capture_microphone(query: &str, sink: AudioSink) -> Result<(MicrophoneCapture, u32)> {
    let device = find_microphone(query)?;
    let name = unsafe { device.localizedName() }.to_string();
    let channels = microphone_channels(&device);

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
    let delegate = AudioDelegate::new(sink, channels);
    let queue = DispatchQueue::new("isocast.microphone-capture", None);
    // Ask AVFoundation for the engine's format, resampled if needed.
    let settings = pcm_settings(channels);
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
    unsafe { session.startRunning() };
    if !unsafe { session.isRunning() } {
        bail!("{name:?} did not start");
    }
    Ok((
        MicrophoneCapture {
            session,
            observers,
            _delegate: delegate,
            _queue: queue,
        },
        channels,
    ))
}

/// The channels recorded from a microphone.
pub(crate) fn microphone_channels(device: &AVCaptureDevice) -> u32 {
    device_channels(device).clamp(1, MAX_CHANNELS)
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
    fn new(sink: AudioSink, channels: u32) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioIvars {
            samples: sink.samples,
            stopped: Arc::from(sink.stopped),
            channels,
            clock: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn deliver(&self, sample: &CMSampleBuffer) {
        let ivars = self.ivars();
        match interleaved_f32(sample, ivars.channels) {
            Ok(Some(samples)) => {
                let time = unsafe { sample.presentation_time_stamp() };
                let clock = ivars.clock.lock().unwrap();
                (ivars.samples)(&samples, host_time(time, clock.as_deref()));
            }
            Ok(None) => {}
            Err(err) => (ivars.stopped)(format!("{err:#}")),
        }
    }
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

/// Start capturing what the computer plays, as stereo, leaving out the
/// apps `exclude` names. `exclude_own` leaves out what this process plays
/// (monitoring).
pub fn capture_system_audio(
    exclude: &[String],
    sink: AudioSink,
    exclude_own: bool,
) -> Result<(SystemAudioCapture, u32)> {
    let backend = tap::backend();
    isocast_diagnostics::diagnostic!("info";
        "system audio: capturing through {}", backend.label());
    match backend {
        Backend::ProcessTap => {
            let target = Target::System {
                exclude: exclude.to_vec(),
                exclude_own,
            };
            let (tap, channels) = tap::capture(target, sink)?;
            Ok((SystemAudioCapture::tap(tap), channels))
        }
        Backend::ScreenCaptureKit => {
            let (stream, channels) = capture_played_audio(None, exclude, sink, exclude_own)?;
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
            let (stream, channels) = capture_played_audio(Some(app), &[], sink, false)?;
            Ok((SystemAudioCapture::stream(stream), channels))
        }
    }
}

/// What the computer (or only `app`) plays, through ScreenCaptureKit,
/// leaving out the running apps `exclude` names.
fn capture_played_audio(
    app: Option<&str>,
    exclude: &[String],
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
                .filter(|a| exclude.iter().any(|q| crate::capture::app_matches(a, q)))
                .collect();
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
    let delegate = AudioDelegate::new(sink, CHANNELS);
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
