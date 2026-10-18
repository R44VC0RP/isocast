//! Camera capture with AVFoundation: built-in and USB cameras, capture
//! cards, Continuity Camera, and iPhone or iPad screens over USB.

use std::ptr::NonNull;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// How long an iPhone or iPad screen may take to send its first frame.
const SCREEN_FIRST_FRAME: Duration = Duration::from_secs(5);

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use isocast_engine::{CameraRequest, CaptureFormat, Time, VideoSink};
use isocast_protocol::Canvas;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat, AVCaptureDeviceInput,
    AVCaptureDeviceWasDisconnectedNotification, AVCaptureOutput, AVCaptureSession,
    AVCaptureSessionErrorKey, AVCaptureSessionRuntimeErrorNotification, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate,
};
use objc2_core_foundation::CFType;
use objc2_core_media::{
    CMClock, CMSampleBuffer, CMSyncConvertTime, CMTime, CMVideoFormatDescriptionGetDimensions,
};
use objc2_core_video::{
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{
    NSDictionary, NSError, NSNotification, NSNotificationCenter, NSNumber, NSObject,
    NSObjectProtocol, NSString,
};

use crate::devices::{allow_device_screens, find_camera, is_device_screen};
use crate::writer::catch;
use crate::{PixelBuffer, from_cm_time};

/// A running camera capture. Dropping it stops the session.
pub struct CameraCapture {
    session: Retained<AVCaptureSession>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    // Kept alive for the output, which holds them weakly.
    _delegate: Retained<VideoDelegate>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: AVCaptureSession may be stopped from any thread; the delegate is
// only called by AVFoundation on its queue.
unsafe impl Send for CameraCapture {}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        unwatch(&self.observers);
        unsafe { self.session.stopRunning() };
    }
}

/// Start capturing the camera `query` names.
pub fn capture_camera(
    query: &str,
    request: CameraRequest,
    canvas: &Canvas,
    sink: VideoSink<PixelBuffer>,
) -> Result<(CameraCapture, CaptureFormat)> {
    allow_device_screens();
    let device = find_camera(query)?;
    let name = unsafe { device.localizedName() }.to_string();
    // An iPhone or iPad screen has no fixed formats; its size is whatever
    // its first frame is.
    let screen = is_device_screen(&device);
    let choice = if screen {
        None
    } else {
        Some(choose_format(&device, request, canvas).with_context(|| format!("camera {name:?}"))?)
    };

    let input =
        unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }.map_err(|err| {
            anyhow!(
                "could not open {name:?}: {} (camera permission: System Settings → Privacy & \
                 Security → Camera)",
                err.localizedDescription()
            )
        })?;
    let session = unsafe { AVCaptureSession::new() };
    let output = unsafe { AVCaptureVideoDataOutput::new() };
    // 4:2:0 video range is what the encoder wants; devices that deliver
    // something else (4:2:2 capture cards, BGRA virtual cameras) are
    // converted on the way, which also hands us buffers from the capture
    // output's own pool: passing a virtual camera's BGRA buffers through
    // held them past its small pool (Studio NDI allows 5) and it stalled.
    let format = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
    // SAFETY: CFString is toll-free bridged with NSString.
    let key: &NSString =
        unsafe { &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString) };
    let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[&*format]);
    let delegate = VideoDelegate::new(sink, name.to_string());
    let queue = DispatchQueue::new("isocast.camera-capture", None);
    unsafe {
        output.setVideoSettings(Some(&settings));
        output.setAlwaysDiscardsLateVideoFrames(true);
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
        added
    })?
    .then_some(())
    .with_context(|| format!("{name:?} can't be added to a capture session"))?;
    let configured = unsafe { device.lockForConfiguration() }.map_err(|err| {
        anyhow!(
            "could not configure {name:?}: {}",
            err.localizedDescription()
        )
    });
    if configured.is_ok() {
        // Some devices reject frame durations; they then run at their own
        // rate, which the frame clock evens out.
        if let Some(choice) = &choice {
            let _ = catch(|| unsafe {
                device.setActiveFormat(&choice.format);
                device.setActiveVideoMinFrameDuration(choice.frame_duration);
                device.setActiveVideoMaxFrameDuration(choice.frame_duration);
            });
        }
        unsafe { device.unlockForConfiguration() };
    }
    unsafe { session.commitConfiguration() };
    configured?;

    *delegate.ivars().clock.lock().unwrap() = unsafe { session.synchronizationClock() };
    let observers = watch(&session, &device, {
        let delegate = delegate.clone();
        move |reason| (delegate.ivars().sink.stopped)(reason)
    });
    unsafe { session.startRunning() };
    if !unsafe { session.isRunning() } {
        bail!("{name:?} did not start");
    }
    let format = match choice {
        Some(choice) => CaptureFormat {
            width: choice.width,
            height: choice.height,
            fps: choice.fps,
        },
        None => {
            let deadline = Instant::now() + SCREEN_FIRST_FRAME;
            let size = loop {
                if let Some(size) = *delegate.ivars().first_size.lock().unwrap() {
                    break size;
                }
                if Instant::now() > deadline {
                    bail!(
                        "{name:?} sent no picture; unlock the device, keep its screen on, and \
                         check that it trusts this Mac"
                    );
                }
                thread::sleep(Duration::from_millis(20));
            };
            CaptureFormat {
                width: size.0,
                height: size.1,
                fps: canvas.fps,
            }
        }
    };
    Ok((
        CameraCapture {
            session,
            observers,
            _delegate: delegate,
            _queue: queue,
        },
        format,
    ))
}

/// Report a session runtime error or the device disconnecting to
/// `stopped`. The returned observers must be passed to [`unwatch`].
pub(crate) fn watch(
    session: &AVCaptureSession,
    device: &AVCaptureDevice,
    stopped: impl Fn(String) + Clone + 'static,
) -> Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>> {
    let center = NSNotificationCenter::defaultCenter();
    let on_error = RcBlock::new({
        let stopped = stopped.clone();
        move |note: NonNull<NSNotification>| {
            let note = unsafe { note.as_ref() };
            let reason = note
                .userInfo()
                .and_then(|info| info.objectForKey(unsafe { AVCaptureSessionErrorKey }))
                .and_then(|err| err.downcast::<NSError>().ok())
                .map(|err| err.localizedDescription().to_string())
                .unwrap_or_else(|| "the capture session failed".into());
            stopped(reason);
        }
    });
    let on_disconnect = RcBlock::new(move |_note: NonNull<NSNotification>| {
        stopped("the device was disconnected".into());
    });
    unsafe {
        vec![
            center.addObserverForName_object_queue_usingBlock(
                Some(AVCaptureSessionRuntimeErrorNotification),
                Some(session),
                None,
                &on_error,
            ),
            center.addObserverForName_object_queue_usingBlock(
                Some(AVCaptureDeviceWasDisconnectedNotification),
                Some(device),
                None,
                &on_disconnect,
            ),
        ]
    }
}

pub(crate) fn unwatch(observers: &[Retained<ProtocolObject<dyn NSObjectProtocol>>]) {
    let center = NSNotificationCenter::defaultCenter();
    for observer in observers {
        let observer: &AnyObject = observer.as_ref();
        unsafe { center.removeObserver(observer) };
    }
}

/// A sample time on `clock` (a capture session's clock) as host time. They
/// are usually the same clock.
pub(crate) fn host_time(time: CMTime, clock: Option<&CMClock>) -> Time {
    let Some(clock) = clock else {
        return from_cm_time(time);
    };
    let host = unsafe { CMClock::host_time_clock() };
    let (from, to): (&CFType, &CFType) = (clock.as_ref(), host.as_ref());
    if from == to {
        return from_cm_time(time);
    }
    from_cm_time(unsafe { CMSyncConvertTime(time, from, to) })
}

pub(crate) struct FormatChoice {
    format: Retained<AVCaptureDeviceFormat>,
    pub width: u32,
    pub height: u32,
    frame_duration: CMTime,
    pub fps: u32,
}

/// The format to capture in. Without a request: the highest frame rate up
/// to the canvas rate (a 1080p60 facecam beats 4K30, 1080p60 beats 720p120),
/// then the largest size that fits on the canvas either way round (else the
/// smallest larger one: no 4:3 1920×1440 on a 1080p canvas when 1920×1080
/// is there; 1920×1080 on a 1080×1920 one), turned as the canvas is if it
/// can be, then a 4:2:0 pixel format, which needs no conversion.
pub(crate) fn choose_format(
    device: &AVCaptureDevice,
    request: CameraRequest,
    canvas: &Canvas,
) -> Result<FormatChoice> {
    let canvas_fps = canvas.fps;
    let target = request.fps.unwrap_or(canvas_fps) as f64;
    // Frame rate, fits, size, turned as the canvas, pixel format.
    type Score = (u32, bool, u64, bool, u8);
    let mut best: Option<(Score, FormatChoice)> = None;
    let mut offered = Vec::new();
    for format in unsafe { device.formats() }.iter() {
        let description = unsafe { format.formatDescription() };
        let size = unsafe { CMVideoFormatDescriptionGetDimensions(&description) };
        let (width, height) = (size.width as u32, size.height as u32);
        let pixel_format = unsafe { description.media_sub_type() };
        let pixel_score = if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            2
        } else if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
            1
        } else {
            0
        };
        for range in unsafe { format.videoSupportedFrameRateRanges() }.iter() {
            let (min, max) = unsafe { (range.minFrameRate(), range.maxFrameRate()) };
            offered.push(format!(
                "{width}×{height} @ {}",
                (max * 100.0).round() / 100.0
            ));
            if request.size.is_some_and(|wanted| wanted != (width, height)) {
                continue;
            }
            // Run at the target rate if the range allows it, else as close
            // as the range gets.
            let (rate, frame_duration) = if (min - 0.01..=max + 0.01).contains(&target) {
                (target, unsafe { CMTime::new(1, target as i32) })
            } else if max < target {
                (max, unsafe { range.minFrameDuration() })
            } else {
                (min, unsafe { range.maxFrameDuration() })
            };
            if request.fps.is_some() && (rate - target).abs() > 0.5 {
                continue;
            }
            let fps = rate.round() as u32;
            let area = width as u64 * height as u64;
            let fits = width.max(height) <= canvas.width.max(canvas.height)
                && width.min(height) <= canvas.width.min(canvas.height);
            let score = (
                fps.min(canvas_fps),
                fits,
                if fits { area } else { u64::MAX - area },
                (width >= height) == (canvas.width >= canvas.height),
                pixel_score,
            );
            if best.as_ref().is_none_or(|(b, _)| score > *b) {
                best = Some((
                    score,
                    FormatChoice {
                        format: format.clone(),
                        width,
                        height,
                        frame_duration,
                        fps,
                    },
                ));
            }
        }
    }
    offered.dedup();
    best.map(|(_, choice)| choice).ok_or_else(|| {
        anyhow!(
            "no format matches {}; the device offers: {}",
            describe(request),
            offered.join(", ")
        )
    })
}

fn describe(request: CameraRequest) -> String {
    match (request.size, request.fps) {
        (Some((w, h)), Some(fps)) => format!("{w}×{h} @ {fps}"),
        (Some((w, h)), None) => format!("{w}×{h}"),
        (None, Some(fps)) => format!("{fps} fps"),
        (None, None) => "any format".into(),
    }
}

pub struct DelegateIvars {
    frames: Mutex<Option<crate::FramePool>>,
    /// The size of the first frame, for devices without fixed formats.
    first_size: Mutex<Option<(u32, u32)>>,
    /// The device's name, and frames the capture output discarded since
    /// the last report (logged at most every 5 s, so a camera losing frames
    /// shows in isocastd's log).
    name: String,
    dropped: Mutex<(Instant, u64)>,
    /// With ISOCAST_CAMERA_STATS set: frames delivered since the last
    /// report, logged every 5 s (to measure a camera's real rate).
    delivered: Option<Mutex<(Instant, u64)>>,
    sink: VideoSink<PixelBuffer>,
    /// The session's clock, which sample times are on.
    clock: Mutex<Option<Retained<CMClock>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and VideoDelegate
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "IsocastCameraOutput"]
    #[ivars = DelegateIvars]
    pub struct VideoDelegate;

    unsafe impl NSObjectProtocol for VideoDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for VideoDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                return;
            };
            let time = unsafe { sample.presentation_time_stamp() };
            let clock = self.ivars().clock.lock().unwrap();
            let mut first = self.ivars().first_size.lock().unwrap();
            if first.is_none() {
                *first = Some((
                    CVPixelBufferGetWidth(&image) as u32,
                    CVPixelBufferGetHeight(&image) as u32,
                ));
            }
            drop(first);
            if let Some(delivered) = &self.ivars().delivered {
                let mut delivered = delivered.lock().unwrap();
                delivered.1 += 1;
                let seconds = delivered.0.elapsed().as_secs_f64();
                if seconds >= 5.0 {
                    isocast_diagnostics::output::deferred(
                        "info",
                        module_path!(),
                        format!(
                            "isocastd: camera {:?}: {:.1} frames/s delivered",
                            self.ivars().name,
                            delivered.1 as f64 / seconds
                        ),
                    );
                    *delivered = (Instant::now(), 0);
                }
            }
            match crate::FramePool::copy(&mut self.ivars().frames.lock().unwrap(), &image) {
                Ok(Some(frame)) => {
                    (self.ivars().sink.frame)(frame, host_time(time, clock.as_deref()))
                }
                Ok(None) => {} // Counted and warned by the pool ledger.
                Err(error) => {
                    (self.ivars().sink.stopped)(format!("camera frame isolation: {error:#}"))
                }
            }
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        fn did_drop(
            &self,
            _output: &AVCaptureOutput,
            _sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            let mut dropped = self.ivars().dropped.lock().unwrap();
            dropped.1 += 1;
            if dropped.0.elapsed() >= Duration::from_secs(5) {
                isocast_diagnostics::output::deferred(
                    "warn",
                    module_path!(),
                    format!(
                        "isocastd: camera {:?}: the capture output discarded {} late frame(s) in {:.0} s",
                        self.ivars().name,
                        dropped.1,
                        dropped.0.elapsed().as_secs_f64()
                    ),
                );
                *dropped = (Instant::now(), 0);
            }
        }
    }
);

impl VideoDelegate {
    fn new(sink: VideoSink<PixelBuffer>, name: String) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars {
            frames: Mutex::new(None),
            first_size: Mutex::new(None),
            name,
            dropped: Mutex::new((Instant::now(), 0)),
            delivered: std::env::var_os("ISOCAST_CAMERA_STATS")
                .map(|_| Mutex::new((Instant::now(), 0))),
            sink,
            clock: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }
}
