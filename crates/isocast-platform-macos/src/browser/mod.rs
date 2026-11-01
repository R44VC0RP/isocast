//! Browser sources: a web page rendered by WebKit in an offscreen window,
//! snapshotted at the source's frame rate into IOSurface frames.
//!
//! WebKit lives on the main thread, so each page is kept there (see
//! [`crate::run_main_queue`]); the capture handle only holds its id.
//!
//! With audio, a script in the page (`audio.js`) routes its sound into a
//! capture context and posts it here, base64-encoded, instead of playing it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use block2::RcBlock;
use isocast_engine::{
    AudioSink, BrowserAudioStamper as Stamper, BrowserControl, BrowserPage, CaptureFormat,
    FramePool as _, PageLocation, SAMPLE_RATE, VideoSink,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSBackingStoreType, NSColor,
    NSImage, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{
    NSError, NSNumber, NSObject, NSObjectNSKeyValueCoding, NSObjectProtocol, NSPoint, NSRect,
    NSSize, NSString, NSTimer, NSURL, NSURLRequest,
};
use objc2_web_kit::{
    WKAudiovisualMediaTypes, WKScriptMessage, WKScriptMessageHandler, WKSnapshotConfiguration,
    WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
    WKWebViewConfiguration,
};

use crate::frames::{FramePool, PixelBuffer};
use crate::{host_time, on_main_thread};

/// How long setting a page up on the main thread may take.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The script that captures a page's sound.
const AUDIO_SCRIPT: &str = concat!(
    "window.webkit.messageHandlers.isocastAudio.postMessage('48000:');\n",
    include_str!("audio.js")
);

/// The message handler `audio.js` posts to.
const AUDIO_HANDLER: &str = "isocastAudio";

const AUDIO_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
const FRAME_BYTES: usize = 2 * size_of::<f32>();

static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The pages being rendered, on the main thread.
    static PAGES: RefCell<HashMap<u64, Page>> = RefCell::new(HashMap::new());
}

/// A page on the main thread: its window, view and frame timer.
struct Page {
    window: Retained<NSWindow>,
    web: Retained<WKWebView>,
    timer: Retained<NSTimer>,
    address: Retained<NSURL>,
    /// Whether the page is loaded, or unloaded while it isn't shown.
    loaded: Rc<Cell<bool>>,
    /// An empty frame has been sent since the page was unloaded.
    cleared: Rc<Cell<bool>>,
}

impl Page {
    fn load(&self) {
        if self.address.isFileURL() {
            // The page may load the files next to it.
            let folder = self
                .address
                .URLByDeletingLastPathComponent()
                .unwrap_or_else(|| self.address.clone());
            unsafe {
                self.web
                    .loadFileURL_allowingReadAccessToURL(&self.address, &folder)
            };
        } else {
            let request = NSURLRequest::requestWithURL(&self.address);
            unsafe { self.web.loadRequest(&request) };
        }
    }

    /// Load the page again, bypassing the cache, if it is loaded.
    fn refresh(&self) {
        if self.loaded.get() {
            unsafe { self.web.reloadFromOrigin() };
        }
    }

    /// Unload the page, leaving an empty frame, or load it again.
    fn set_loaded(&self, loaded: bool) {
        if self.loaded.replace(loaded) == loaded {
            return;
        }
        if loaded {
            self.load();
        } else {
            self.cleared.set(false);
            unsafe { self.web.loadHTMLString_baseURL(&NSString::new(), None) };
        }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.timer.invalidate();
        // The handler holds the audio sink; let it go with the page.
        unsafe {
            self.web
                .configuration()
                .userContentController()
                .removeAllScriptMessageHandlers()
        };
        self.window.close();
    }
}

/// Control of a page from other threads.
struct PageControl {
    id: u64,
}

impl PageControl {
    fn with_page(&self, action: impl FnOnce(&Page) + Send + 'static) {
        let id = self.id;
        on_main_thread(SETUP_TIMEOUT, move || {
            PAGES.with(|pages| pages.borrow().get(&id).map(action));
        });
    }
}

impl BrowserControl for PageControl {
    fn refresh(&self) {
        self.with_page(Page::refresh);
    }

    fn set_loaded(&self, loaded: bool) {
        self.with_page(move |page| page.set_loaded(loaded));
    }
}

/// A running browser source. Dropping it closes the page.
pub struct BrowserCapture {
    id: u64,
}

impl Drop for BrowserCapture {
    fn drop(&mut self) {
        let id = self.id;
        on_main_thread(SETUP_TIMEOUT, move || {
            PAGES.with(|pages| pages.borrow_mut().remove(&id));
        });
    }
}

/// Start rendering `page`.
pub fn capture_browser(
    page: &BrowserPage<'_>,
    sink: VideoSink<PixelBuffer>,
    audio: Option<AudioSink>,
) -> Result<(BrowserCapture, CaptureFormat, Arc<dyn BrowserControl>)> {
    let (width, height, fps) = (page.width, page.height, page.fps);
    let pool = FramePool::new(width, height)?;
    let id = NEXT_PAGE.fetch_add(1, Ordering::Relaxed);
    let location = match page.location {
        PageLocation::Url(url) => Location::Url(url.to_string()),
        PageLocation::File(path) => Location::File(path.to_string_lossy().into_owned()),
    };
    let css = page.css.to_string();
    let sink = Arc::new(sink);
    // The pool moves to the main thread, where frames are drawn.
    let pool = Mutex::new(Some(pool));
    on_main_thread(SETUP_TIMEOUT, move || -> Result<()> {
        let mtm = MainThreadMarker::new().context("not on the main thread")?;
        let pool = pool.lock().unwrap().take().context("set up twice")?;
        let page = open_page(mtm, &location, &css, width, height, fps, pool, sink, audio)?;
        PAGES.with(|pages| pages.borrow_mut().insert(id, page));
        Ok(())
    })
    .context("the main thread did not answer; browser sources need the daemon")??;
    Ok((
        BrowserCapture { id },
        CaptureFormat { width, height, fps },
        Arc::new(PageControl { id }),
    ))
}

/// [`PageLocation`] that can cross to the main thread.
enum Location {
    Url(String),
    File(String),
}

#[allow(clippy::too_many_arguments)]
fn open_page(
    mtm: MainThreadMarker,
    location: &Location,
    css: &str,
    width: u32,
    height: u32,
    fps: u32,
    pool: FramePool,
    sink: Arc<VideoSink<PixelBuffer>>,
    audio: Option<AudioSink>,
) -> Result<Page> {
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    // Media plays without a click, as in OBS.
    unsafe { config.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::None) };
    // A local page may fetch the files next to it, as in OBS (WebKit
    // otherwise refuses file URLs to file pages).
    unsafe {
        config.preferences().setValue_forKey(
            Some(&NSNumber::new_bool(true)),
            &NSString::from_str("allowFileAccessFromFileURLs"),
        )
    };
    if let Some(sink) = audio {
        let controller = unsafe { config.userContentController() };
        let handler = PageAudio::new(mtm, sink);
        unsafe {
            controller.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*handler),
                &NSString::from_str(AUDIO_HANDLER),
            );
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(AUDIO_SCRIPT),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
            );
            controller.addUserScript(&script);
        }
    }
    // The custom CSS, added once the document is there.
    if !css.trim().is_empty() {
        let quoted = serde_json_string(css);
        let source = format!(
            "(function(){{var s=document.createElement('style');s.textContent={quoted};\
             (document.head||document.documentElement).appendChild(s);}})();"
        );
        let script = unsafe {
            WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(&source),
                WKUserScriptInjectionTime::AtDocumentEnd,
                true,
            )
        };
        unsafe { config.userContentController().addUserScript(&script) };
    }
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width.into(), height.into()),
    );
    let web =
        unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config) };
    // Transparent where the page doesn't paint.
    unsafe {
        web.setValue_forKey(
            Some(&NSNumber::new_bool(false)),
            &NSString::from_str("drawsBackground"),
        )
    };
    // Off screen, so nobody sees it; WebKit is told to keep painting.
    let window_frame = NSRect::new(
        NSPoint::new(-20_000.0, -20_000.0),
        NSSize::new(width.into(), height.into()),
    );
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            window_frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    // Pages look the same whether this Mac is in light or dark mode
    // (`prefers-color-scheme` is light, a page's default).
    window.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua }).as_deref());
    window.setContentView(Some(&web));
    window.orderFrontRegardless();
    let occlusion = sel!(_setWindowOcclusionDetectionEnabled:);
    let responds: bool = unsafe { msg_send![&*web, respondsToSelector: occlusion] };
    if responds {
        let _: () = unsafe { msg_send![&*web, _setWindowOcclusionDetectionEnabled: false] };
    }

    let address = match location {
        Location::Url(url) => NSURL::URLWithString(&NSString::from_str(url))
            .with_context(|| format!("{url:?} is not a URL"))?,
        Location::File(path) => NSURL::fileURLWithPath(&NSString::from_str(path)),
    };

    // Snapshot at the frame rate once the page has loaded, skipping a tick
    // while a snapshot is in flight. An unloaded page leaves one empty
    // frame.
    let pool = Rc::new(RefCell::new(pool));
    let busy = Rc::new(Cell::new(false));
    let reported = Rc::new(Cell::new(false));
    let loaded = Rc::new(Cell::new(true));
    let cleared = Rc::new(Cell::new(false));
    let tick = RcBlock::new({
        let (web, window) = (web.clone(), window.clone());
        let (loaded, cleared) = (loaded.clone(), cleared.clone());
        move |_timer: NonNull<NSTimer>| {
            if busy.get() {
                return;
            }
            if !loaded.get() {
                if !cleared.replace(true) {
                    let empty = pool.borrow_mut().draw(&mut |pixels, _| pixels.fill(0));
                    if let Ok(Some(frame)) = empty {
                        (sink.frame)(frame, host_time());
                    }
                }
                return;
            }
            if unsafe { web.isLoading() } {
                return;
            }
            busy.set(true);
            let config = unsafe { WKSnapshotConfiguration::new(mtm) };
            // One snapshot pixel per frame pixel, whatever the window's
            // backing scale (2 on Retina screens, which costs four times
            // as much to draw and then to scale down).
            let points = f64::from(width) / window.backingScaleFactor();
            unsafe { config.setSnapshotWidth(Some(&NSNumber::new_f64(points))) };
            let (busy, pool, sink) = (busy.clone(), pool.clone(), sink.clone());
            let reported = reported.clone();
            let done = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                busy.set(false);
                let drawn = match unsafe { image.as_ref() } {
                    Some(image) => draw(&mut pool.borrow_mut(), image, width, height),
                    None => Err(match unsafe { error.as_ref() } {
                        Some(error) => anyhow!("{}", error.localizedDescription()),
                        None => anyhow!("no picture"),
                    }),
                };
                match drawn {
                    Ok(Some(frame)) => (sink.frame)(frame, host_time()),
                    // The pool is out of frames: the consumer is behind.
                    Ok(None) => {}
                    Err(err) => {
                        if !reported.replace(true) {
                            isocast_diagnostics::diagnostic!("warn";
                                "isocastd: browser source snapshot failed: {err:#}"
                            );
                        }
                    }
                }
            });
            unsafe { web.takeSnapshotWithConfiguration_completionHandler(Some(&config), &done) };
        }
    });
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / f64::from(fps), true, &tick)
    };
    let page = Page {
        window,
        web,
        timer,
        address,
        loaded,
        cleared,
    };
    page.load();
    Ok(page)
}

/// Draw a snapshot into a frame from `pool`, scaled to `width`×`height`,
/// with straight alpha.
fn draw(
    pool: &mut FramePool,
    image: &NSImage,
    width: u32,
    height: u32,
) -> Result<Option<PixelBuffer>> {
    let cg_image =
        unsafe { image.CGImageForProposedRect_context_hints(ptr::null_mut(), None, None) }
            .ok_or_else(|| anyhow!("the snapshot has no bitmap"))?;
    let mut failed = None;
    let frame = pool.draw(&mut |pixels, stride| {
        let space = CGColorSpace::new_device_rgb();
        let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0;
        let context = unsafe {
            CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                width as usize,
                height as usize,
                8,
                stride,
                space.as_deref(),
                info,
            )
        };
        let Some(context) = context else {
            failed = Some(anyhow!("could not draw the snapshot"));
            return;
        };
        let rect = objc2_core_foundation::CGRect::new(
            objc2_core_foundation::CGPoint::new(0.0, 0.0),
            objc2_core_foundation::CGSize::new(width.into(), height.into()),
        );
        CGContext::clear_rect(Some(&context), rect);
        CGContext::draw_image(Some(&context), rect, Some(&cg_image));
        drop(context);
        // Frames carry straight alpha.
        for row in pixels.chunks_mut(stride).take(height as usize) {
            for pixel in row[..width as usize * 4].as_chunks_mut::<4>().0 {
                let a = u32::from(pixel[3]);
                if a != 0 && a != 255 {
                    for c in &mut pixel[..3] {
                        *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
        }
    })?;
    if let Some(err) = failed {
        bail!(err);
    }
    Ok(frame)
}

/// `text` as a JavaScript string literal.
fn serde_json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Where a page's audio messages go: decoded, stamped and delivered.
struct PageAudioIvars {
    sink: AudioSink,
    stamper: RefCell<Stamper>,
    resampler: RefCell<Resampler>,
    reported: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and PageAudio
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IsocastPageAudio"]
    #[ivars = PageAudioIvars]
    struct PageAudio;

    unsafe impl NSObjectProtocol for PageAudio {}

    unsafe impl WKScriptMessageHandler for PageAudio {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let now = crate::host_time();
            let body = unsafe { message.body() };
            let decoded = body
                .downcast::<NSString>()
                .map_err(|_| anyhow!("not a string"))
                .and_then(|text| decode(&text));
            match decoded {
                Ok((rate, samples)) => {
                    // The document-start script sends an empty first record.
                    // Ordered with its audio, even on page-initiated reload.
                    if samples.is_empty() {
                        self.ivars().stamper.replace(Default::default());
                        self.ivars().resampler.replace(Default::default());
                        return;
                    }
                    let samples = self.ivars().resampler.borrow_mut().run(rate, samples);
                    let frames = samples.len() / 2;
                    if frames > 0 {
                        let (at, gap) = self.ivars().stamper.borrow_mut().stamp(frames, now);
                        if let Some(gap) = gap {
                            (self.ivars().sink.report_gap)(at, gap);
                        }
                        (self.ivars().sink.samples)(&samples, at);
                    }
                }
                Err(err) => {
                    if !self.ivars().reported.replace(true) {
                        isocast_diagnostics::diagnostic!("warn";
                            "isocastd: a browser source sent unreadable audio: {err:#}"
                        );
                    }
                }
            }
        }
    }
);

impl PageAudio {
    fn new(mtm: MainThreadMarker, sink: AudioSink) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PageAudioIvars {
            sink,
            stamper: RefCell::new(Stamper::default()),
            resampler: RefCell::new(Resampler::default()),
            reported: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// A message from `audio.js`: `RATE:BASE64`, the base64 of interleaved
/// stereo 32-bit floats.
fn decode(message: &NSString) -> Result<(u32, Vec<f32>)> {
    // Bound the native string before copying it. RATE (u32), colon and
    // padded base64 are ASCII, so each occupies one UTF-16 unit per byte.
    let max_bytes = *AUDIO_RATES.end() as usize * FRAME_BYTES;
    if message.length() > 11 + max_bytes.div_ceil(3) * 4 {
        bail!("oversized browser audio message");
    }
    let message = message.to_string();
    let (rate, data) = message.split_once(':').context("no sample rate")?;
    let rate: u32 = rate.parse().context("bad sample rate")?;
    if !AUDIO_RATES.contains(&rate) {
        bail!("unsupported browser audio rate {rate} Hz (accepted: {AUDIO_RATES:?} Hz)");
    }
    let second = rate as usize * FRAME_BYTES;
    if data.len() > second.div_ceil(3) * 4 {
        bail!("browser audio exceeds one second at {rate} Hz");
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
    if bytes.len() > second || !bytes.len().is_multiple_of(FRAME_BYTES) {
        bail!("expected whole stereo frames and at most one second of browser audio");
    }
    let samples = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    Ok((rate, samples))
}

/// Linear resampling of interleaved stereo to [`SAMPLE_RATE`], for pages
/// whose capture context runs at another rate.
#[derive(Default)]
struct Resampler {
    /// Position of the next output frame, in input frames after `last`.
    position: f64,
    last: [f32; 2],
}

impl Resampler {
    fn run(&mut self, rate: u32, samples: Vec<f32>) -> Vec<f32> {
        let input = samples.as_chunks::<2>().0;
        if rate == SAMPLE_RATE || input.is_empty() {
            return samples;
        }
        let step = f64::from(rate) / f64::from(SAMPLE_RATE);
        let last = self.last;
        // Frame -1 is the previous chunk's last.
        let frame = |i: isize| if i < 0 { last } else { input[i as usize] };
        let mut out = Vec::with_capacity((input.len() as f64 / step) as usize * 2 + 2);
        while self.position < (input.len() - 1) as f64 {
            let i = self.position.floor();
            let t = (self.position - i) as f32;
            let (a, b) = (frame(i as isize), frame(i as isize + 1));
            out.extend([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.position += step;
        }
        self.position -= input.len() as f64;
        self.last = input[input.len() - 1];
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_decode_to_samples() {
        let samples = [0.5f32, -0.25, 1.0, 0.0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let message = format!(
            "44100:{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let (rate, decoded) = decode(&NSString::from_str(&message)).unwrap();
        assert_eq!(decoded, samples);
        let mut resampler = Resampler::default();
        let out = resampler.run(rate, decoded);
        assert_eq!(out.len(), 4);
        assert_eq!(&out[..2], &samples[..2]);
        assert!((out[2] - 0.959375).abs() < 1e-6);
        assert!((out[3] - -0.0203125).abs() < 1e-6);
        let message = message.replacen("44100:", "48000:", 1);
        let (rate, decoded) = decode(&NSString::from_str(&message)).unwrap();
        assert_eq!(resampler.run(rate, decoded), samples);
        assert!(decode(&NSString::from_str("no rate")).is_err());
    }

    #[test]
    fn browser_audio_bounds_apply_before_resampling() {
        let message = |rate, bytes| {
            let data = base64::engine::general_purpose::STANDARD.encode(vec![0; bytes]);
            NSString::from_str(&format!("{rate}:{data}"))
        };
        for rate in [0, 1, 7_999, 192_001, u32::MAX] {
            assert!(decode(&message(rate, FRAME_BYTES)).is_err(), "{rate} Hz");
        }
        for rate in [8_000, 192_000] {
            let second = rate as usize * FRAME_BYTES;
            assert_eq!(decode(&message(rate, second)).unwrap().1.len(), second / 4);
            assert!(decode(&message(rate, second + FRAME_BYTES)).is_err());
        }
        // One extra byte can fit within the same padded base64 length.
        assert!(decode(&message(8_000, 64_001)).is_err());
        assert!(decode(&message(48_000, FRAME_BYTES - 1)).is_err());
    }

    #[test]
    fn bursts_are_stamped_back_to_back_until_they_drift() {
        let ms = |n: i64| isocast_engine::Time::from_nanos(n * 1_000_000);
        let mut stamper = Stamper::default();
        // 480 frames (10 ms) arriving at 100 ms: made at 90 ms.
        assert_eq!(stamper.stamp(480, ms(100)), (ms(90), None));
        // The next arrives 30 ms late but follows on.
        assert_eq!(stamper.stamp(480, ms(140)), (ms(100), None));
        // One 500 ms later starts again from when it arrived.
        assert_eq!(stamper.stamp(480, ms(640)), (ms(630), None));
    }

    #[test]
    fn resampling_is_continuous_across_chunks() {
        let mut resampler = Resampler::default();
        // A ramp at 24 kHz comes out at 48 kHz with points in between.
        let ramp: Vec<f32> = (0..200).flat_map(|i| [i as f32, i as f32]).collect();
        let mut out = Vec::new();
        for chunk in ramp.chunks(50) {
            out.extend(resampler.run(24_000, chunk.to_vec()));
        }
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!((396..=400).contains(&left.len()), "{} frames", left.len());
        for pair in left.windows(2) {
            assert!((pair[1] - pair[0] - 0.5).abs() < 1e-4, "{pair:?}");
        }
        // 48 kHz passes through untouched.
        assert_eq!(resampler.run(48_000, vec![1.0, 2.0]), vec![1.0, 2.0]);
    }
}
