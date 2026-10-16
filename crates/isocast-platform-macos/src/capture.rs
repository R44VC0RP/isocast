//! Display capture with ScreenCaptureKit.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use isocast_engine::{CaptureFormat, ScreenTarget, Time, VideoSink};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFArray;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyAllDisplayModes, CGDisplayMode, kCGColorSpaceITUR_709,
    kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCRunningApplication, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType, SCWindow,
};

use crate::devices::{find_display, shareable_content};
use crate::{PixelBuffer, from_cm_time};

/// Frames ScreenCaptureKit may have in flight. The maximum, because the
/// frame clock and the encoder each hold a few.
const QUEUE_DEPTH: isize = 8;
/// IOKit's flag for a display's native (panel) mode.
const NATIVE_MODE_FLAG: u32 = 0x0200_0000;
/// How long starting or stopping a stream may take.
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// A running display capture. Dropping it stops the stream.
pub struct DisplayCapture {
    stream: Retained<SCStream>,
    // Kept alive for the stream, which holds them weakly.
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: SCStream may be stopped from any thread; the handler objects are
// only used by ScreenCaptureKit on its queue.
unsafe impl Send for DisplayCapture {}

impl Drop for DisplayCapture {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = rx.recv_timeout(STREAM_TIMEOUT);
    }
}

/// Start capturing `target` at its native resolution and up to `fps`.
pub fn capture_screen(
    target: ScreenTarget<'_>,
    fps: u32,
    ten_bit: bool,
    cursor: bool,
    sink: VideoSink<PixelBuffer>,
) -> Result<(DisplayCapture, CaptureFormat)> {
    let content = shareable_content().context(
        "screen recording permission is needed: System Settings → Privacy & Security → \
         Screen & System Audio Recording",
    )?;
    let display = |name: &str| -> Result<(Retained<SCDisplay>, u32, u32, String)> {
        let (id, info) = find_display(name)?;
        let sc_display = unsafe { content.displays() }
            .iter()
            .find(|d| unsafe { d.displayID() } == id)
            .with_context(|| format!("ScreenCaptureKit can't see display {:?}", info.name))?;
        let (width, height) = native_size(id).unwrap_or((info.width, info.height));
        Ok((sc_display, width, height, info.name))
    };
    let window = matches!(target, ScreenTarget::Window { .. });
    let (filter, width, height, label) = match target {
        ScreenTarget::Display(name) => {
            let (sc_display, width, height, label) = display(name)?;
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &sc_display,
                    &NSArray::new(),
                )
            };
            (filter, width, height, label)
        }
        ScreenTarget::Window { app, title } => {
            let window = find_window(&content, app, title)?;
            let filter = unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            };
            // The window's size in pixels, even for the encoder.
            let (rect, scale) = unsafe { (filter.contentRect(), filter.pointPixelScale()) };
            let even = |v: f64| ((v * f64::from(scale)).round() as u32).max(2) & !1;
            let label =
                unsafe { window.title() }.map_or_else(|| app.to_string(), |t| t.to_string());
            (filter, even(rect.size.width), even(rect.size.height), label)
        }
        ScreenTarget::Application { app, display: name } => {
            let (sc_display, width, height, _) = display(name)?;
            let apps = find_applications(&content, app)?;
            let filter = unsafe {
                SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                    SCContentFilter::alloc(),
                    &sc_display,
                    &apps,
                    &NSArray::new(),
                )
            };
            (filter, width, height, app.to_string())
        }
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setMinimumFrameInterval(CMTime::new(1, fps as i32));
        // 4:2:0 video range is what the encoder wants, so frames go to it
        // without a conversion. Tagged and converted as Rec. 709.
        config.setPixelFormat(if ten_bit {
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        } else {
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        });
        config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
        config.setColorSpaceName(kCGColorSpaceITUR_709);
        config.setShowsCursor(cursor);
        config.setQueueDepth(QUEUE_DEPTH);
        if window {
            // A resized window is scaled to keep the file's size.
            config.setScalesToFit(true);
            config.setPreservesAspectRatio(true);
        }
    }

    let output = StreamOutput::new(sink);
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    let queue = DispatchQueue::new("isocast.display-capture", None);
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|err| {
        anyhow!(
            "could not attach to the stream: {}",
            err.localizedDescription()
        )
    })?;

    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let error = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(error);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match rx.recv_timeout(STREAM_TIMEOUT) {
        Ok(None) => {}
        Ok(Some(error)) => bail!("could not start capturing {label:?}: {error}"),
        Err(_) => bail!("capturing {label:?} did not start in time"),
    }

    Ok((
        DisplayCapture {
            stream,
            _output: output,
            _queue: queue,
        },
        CaptureFormat { width, height, fps },
    ))
}

/// Whether an app's name or bundle id matches `query`: exactly, or the
/// name contains it, ignoring case.
pub(crate) fn app_matches(app: &SCRunningApplication, query: &str) -> bool {
    let query = query.to_lowercase();
    let name = unsafe { app.applicationName() }.to_string().to_lowercase();
    let bundle = unsafe { app.bundleIdentifier() }.to_string().to_lowercase();
    name == query || bundle == query || name.contains(&query)
}

/// The largest on-screen window of `app` whose title contains `title`.
pub(crate) fn find_window(
    content: &SCShareableContent,
    app: &str,
    title: Option<&str>,
) -> Result<Retained<SCWindow>> {
    let windows = unsafe { content.windows() };
    let title = title.map(str::to_lowercase);
    let best = windows
        .iter()
        .filter(|w| unsafe { w.windowLayer() } == 0 && !crate::devices::is_ours(w))
        .filter(|w| unsafe { w.owningApplication() }.is_some_and(|a| app_matches(&a, app)))
        .filter(|w| {
            title.as_ref().is_none_or(|t| {
                unsafe { w.title() }.is_some_and(|wt| wt.to_string().to_lowercase().contains(t))
            })
        })
        .max_by_key(|w| {
            let frame = unsafe { w.frame() };
            (frame.size.width * frame.size.height) as u64
        });
    best.with_context(|| {
        let mut choices: Vec<String> = windows
            .iter()
            .filter(|w| unsafe { w.windowLayer() } == 0 && !crate::devices::is_ours(w))
            .filter_map(|w| {
                let app = unsafe { w.owningApplication() }?;
                let name = unsafe { app.applicationName() }.to_string();
                Some(
                    match unsafe { w.title() }
                        .map(|t| t.to_string())
                        .filter(|t| !t.is_empty())
                    {
                        Some(title) => format!("{name} — {title}"),
                        None => name,
                    },
                )
            })
            .collect();
        choices.sort();
        choices.dedup();
        format!(
            "no window of {app:?}{} is on screen; windows: {}",
            title
                .as_ref()
                .map_or(String::new(), |t| format!(" titled {t:?}")),
            choices.join(", ")
        )
    })
}

/// The running apps matching `app`.
pub(crate) fn find_applications(
    content: &SCShareableContent,
    app: &str,
) -> Result<Retained<NSArray<SCRunningApplication>>> {
    let apps: Vec<Retained<SCRunningApplication>> = unsafe { content.applications() }
        .iter()
        .filter(|a| app_matches(a, app))
        .collect();
    if apps.is_empty() {
        let mut names: Vec<String> = unsafe { content.applications() }
            .iter()
            .map(|a| unsafe { a.applicationName() }.to_string())
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        names.dedup();
        bail!("no running app matches {app:?}; apps: {}", names.join(", "));
    }
    Ok(NSArray::from_retained_slice(&apps))
}

/// The panel's own resolution, which differs from the current mode's
/// pixel size when macOS scales ("looks like 2560×1440" on a 4K panel).
pub(crate) fn native_size(display: CGDirectDisplayID) -> Option<(u32, u32)> {
    let modes = unsafe { CGDisplayCopyAllDisplayModes(display, None) }?;
    // SAFETY: the array holds CGDisplayModes.
    let modes: &CFArray<CGDisplayMode> = unsafe { modes.cast_unchecked() };
    modes
        .iter()
        .find(|mode| CGDisplayMode::io_flags(Some(mode)) & NATIVE_MODE_FLAG != 0)
        .map(|mode| {
            (
                CGDisplayMode::pixel_width(Some(&mode)) as u32,
                CGDisplayMode::pixel_height(Some(&mode)) as u32,
            )
        })
}

pub struct OutputIvars {
    sink: VideoSink<PixelBuffer>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and StreamOutput
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "IsocastStreamOutput"]
    #[ivars = OutputIvars]
    pub struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen
                && let Some((frame, at)) = complete_frame(sample)
            {
                (self.ivars().sink.frame)(frame, at);
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            (self.ivars().sink.stopped)(error.localizedDescription().to_string());
        }
    }
);

impl StreamOutput {
    fn new(sink: VideoSink<PixelBuffer>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { sink });
        unsafe { msg_send![super(this), init] }
    }
}

/// The image and capture time of a sample buffer that holds a new frame.
/// ScreenCaptureKit also sends buffers that only say nothing changed.
fn complete_frame(sample: &CMSampleBuffer) -> Option<(PixelBuffer, Time)> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    // SAFETY: CFArray and CFDictionary are toll-free bridged with their
    // Foundation counterparts, and the attachments are dictionaries.
    let attachments: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*attachments as *const CFArray).cast() };
    let info = attachments.firstObject()?;
    let status = info.objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    let status = status.downcast::<NSNumber>().ok()?;
    if status.integerValue() != SCFrameStatus::Complete.0 {
        return None;
    }
    let image = unsafe { sample.image_buffer() }?;
    let at = from_cm_time(unsafe { sample.presentation_time_stamp() });
    Some((PixelBuffer(image), at))
}
