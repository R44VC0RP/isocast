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
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use block2::RcBlock;
use isocast_engine::{
    AudioSink, BrowserAudioStamper as Stamper, BrowserAudioTiming, BrowserControl, BrowserPage,
    CaptureFormat, FramePool as _, PageLocation, SAMPLE_RATE, Time, VideoSink,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSBackingStoreType, NSColor,
    NSImage, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{
    NSBundle, NSError, NSNumber, NSObject, NSObjectNSKeyValueCoding, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString, NSURL, NSURLRequest,
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

/// Parse WebKit's `CFBundleShortVersionString` major number (e.g. `"21624"` or
/// `"22625.1"`).
fn parse_webkit_short_version(raw: &str) -> Option<u32> {
    raw.trim()
        .split('.')
        .next()?
        .parse::<u32>()
        .ok()
        .filter(|&v| v > 0)
}

/// Read the loaded `WebKit.framework`'s `CFBundleShortVersionString` once per
/// process so `audio.js` can match `MTAudioProcessingTap`'s pre-roll (`~100 ms`
/// on WebKit < 22625 vs `~15 ms` on WebKit >= 22625). Logs once and returns
/// `None` (selecting 15 ms) if unreadable.
fn webkit_short_version() -> Option<u32> {
    static VERSION: OnceLock<Option<u32>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let key = NSString::from_str("CFBundleShortVersionString");
        let bundle = unsafe { NSBundle::bundleForClass(WKWebView::class()) };
        let raw = bundle
            .objectForInfoDictionaryKey(&key)
            .and_then(|v| v.downcast::<NSString>().ok())
            .map(|s| s.to_string());
        let parsed = raw.as_deref().and_then(parse_webkit_short_version);
        if parsed.is_none() {
            isocast_diagnostics::diagnostic!("warn";
                "isocastd: could not read WebKit CFBundleShortVersionString ({raw:?}); using 15 ms media delay"
            );
        }
        parsed
    })
}

/// The script that captures a page's sound, prefixed with Apple WebKit's
/// framework version for `createMediaElementSource` (`MTAudioProcessingTap`)
/// pre-roll compensation.
fn audio_script_source() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT.get_or_init(|| {
        let version_js = match webkit_short_version() {
            Some(v) => v.to_string(),
            None => "null".to_string(),
        };
        format!(
            "window.__isocastAppleWebKit = true;\n\
             window.__isocastAppleWebKitVersion = {version_js};\n\
             window.webkit.messageHandlers.isocastAudio.postMessage('48000:');\n\
             {}",
            include_str!("audio.js")
        )
    })
}

/// The message handler `audio.js` posts to.
const AUDIO_HANDLER: &str = "isocastAudio";

const AUDIO_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
const FRAME_BYTES: usize = 2 * size_of::<f32>();

static NEXT_PAGE: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The pages being rendered, on the main thread.
    static PAGES: RefCell<HashMap<u64, Page>> = RefCell::new(HashMap::new());
}

/// A page on the main thread: its window, view and snapshot state.
struct Page {
    window: Retained<NSWindow>,
    web: Retained<WKWebView>,
    address: Retained<NSURL>,
    snapshotter: Rc<Snapshotter>,
    audio: Option<Retained<PageAudio>>,
    /// Whether the page is loaded, or unloaded while it isn't shown.
    loaded: Rc<Cell<bool>>,
    /// An empty frame has been sent since the page was unloaded.
    cleared: Rc<Cell<bool>>,
    closed: Arc<AtomicBool>,
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
        self.closed.store(true, Ordering::SeqCst);
        if let Some(audio) = self.audio.take() {
            audio.flush(crate::host_time());
        }
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
    closed: Arc<AtomicBool>,
}

impl Drop for BrowserCapture {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
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
    let closed = Arc::new(AtomicBool::new(false));
    let page_closed = closed.clone();
    // The pool moves to the main thread, where frames are drawn.
    let pool = Mutex::new(Some(pool));
    on_main_thread(SETUP_TIMEOUT, move || -> Result<()> {
        let mtm = MainThreadMarker::new().context("not on the main thread")?;
        let pool = pool.lock().unwrap().take().context("set up twice")?;
        let page = open_page(
            mtm,
            &location,
            &css,
            width,
            height,
            fps,
            pool,
            sink,
            audio,
            page_closed,
        )?;
        PAGES.with(|pages| pages.borrow_mut().insert(id, page));
        Ok(())
    })
    .context("the main thread did not answer; browser sources need the daemon")??;
    let pacer_closed = closed.clone();
    let tick_fps = fps.max(1);
    let timeline_epoch = page.epoch.clone();
    std::thread::Builder::new()
        .name(format!("browser-pacer-{id}"))
        .spawn(move || {
            let mut schedule = SnapshotSchedule::new(host_time(), tick_fps);
            loop {
                let grid = timeline_epoch.as_ref().and_then(|cell| cell.get().copied());
                let deadline = schedule.next_deadline(host_time(), grid);
                crate::pacing::sleep_until(deadline);
                if pacer_closed.load(Ordering::SeqCst) {
                    break;
                }
                let tick_closed = pacer_closed.clone();
                dispatch2::DispatchQueue::main().exec_async(move || {
                    if tick_closed.load(Ordering::SeqCst) {
                        return;
                    }
                    PAGES.with(|pages| {
                        if let Some(page) = pages.borrow().get(&id)
                            && let Some(mtm) = MainThreadMarker::new()
                        {
                            page.snapshotter.tick(mtm);
                        }
                    });
                });
            }
        })
        .context("starting browser snapshot pacer")?;
    Ok((
        BrowserCapture { id, closed },
        CaptureFormat { width, height, fps },
        Arc::new(PageControl { id }),
    ))
}

/// [`PageLocation`] that can cross to the main thread.
enum Location {
    Url(String),
    File(String),
}

fn grid_deadline(grid: Time, slot: i64, fps: u32) -> Time {
    let half_ns = 500_000_000i128 / i128::from(fps);
    let offset_ns = slot as i128 * 1_000_000_000 / i128::from(fps) + half_ns;
    Time::from_nanos((grid.as_nanos() as i128 + offset_ns) as i64)
}

/// Paces browser snapshots at `fps`, phase-aligning to `grid + 1 / (2 * fps)`
/// once the session timeline's epoch is known so each snapshot's `captured`
/// timestamp (`request_time - 1 / (2 * fps)`) lands on the recorder slot
/// boundary (`grid.add_ticks(k, fps)`) plus main-queue dispatch latency.
struct SnapshotSchedule {
    fps: u32,
    fallback_origin: Time,
    fallback_step: u64,
    aligned: Option<(Time, i64)>,
}

impl SnapshotSchedule {
    fn new(origin: Time, fps: u32) -> Self {
        Self {
            fps: fps.max(1),
            fallback_origin: origin,
            fallback_step: 0,
            aligned: None,
        }
    }

    fn next_deadline(&mut self, now: Time, grid: Option<Time>) -> Time {
        if self.aligned.is_none()
            && let Some(g) = grid
        {
            let min_next = now + Duration::from_nanos(250_000_000 / u64::from(self.fps));
            let mut slot = min_next.ticks_since(g, self.fps) - 2;
            while grid_deadline(g, slot, self.fps) < min_next {
                slot += 1;
            }
            self.aligned = Some((g, slot));
        }
        if let Some((g, ref mut slot)) = self.aligned {
            let mut target = grid_deadline(g, *slot, self.fps);
            *slot += 1;
            if target <= now {
                let mut jump = now.ticks_since(g, self.fps) - 1;
                while grid_deadline(g, jump, self.fps) <= now {
                    jump += 1;
                }
                target = grid_deadline(g, jump, self.fps);
                *slot = jump + 1;
            }
            target
        } else {
            self.fallback_step += 1;
            let mut target = self.fallback_origin.add_ticks(self.fallback_step, self.fps);
            if target <= now {
                let mut jump = now.ticks_since(self.fallback_origin, self.fps).max(1) as u64;
                while self.fallback_origin.add_ticks(jump, self.fps) <= now {
                    jump += 1;
                }
                self.fallback_step = jump;
                target = self.fallback_origin.add_ticks(jump, self.fps);
            }
            target
        }
    }
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
    closed: Arc<AtomicBool>,
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
    let page_audio = if let Some(sink) = audio {
        let controller = unsafe { config.userContentController() };
        let handler = PageAudio::new(mtm, sink);
        unsafe {
            controller.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*handler),
                &NSString::from_str(AUDIO_HANDLER),
            );
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(audio_script_source()),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
            );
            controller.addUserScript(&script);
        }
        Some(handler)
    } else {
        None
    };
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

    // Snapshot at the source's frame rate once the page has loaded, skipping a
    // tick while a snapshot is in flight (no back-to-back retry on slow hosts).
    // Video reference point: `host_time()` when `takeSnapshotWithConfiguration`
    // is requested minus half of the snapshot polling interval (`1 / (2 * fps)`).
    let loaded = Rc::new(Cell::new(true));
    let cleared = Rc::new(Cell::new(false));
    let snapshotter = Rc::new(Snapshotter {
        web: web.clone(),
        window: window.clone(),
        width,
        height,
        half_interval: Duration::from_nanos(500_000_000 / u64::from(fps.max(1))),
        loaded: loaded.clone(),
        cleared: cleared.clone(),
        closed: closed.clone(),
        busy: Cell::new(false),
        reported: Cell::new(false),
        last_frame: Cell::new(None),
        pool: RefCell::new(pool),
        sink,
    });
    let page = Page {
        window,
        web,
        address,
        snapshotter,
        audio: page_audio,
        loaded,
        cleared,
        closed,
    };
    page.load();
    Ok(page)
}

struct Snapshotter {
    web: Retained<WKWebView>,
    window: Retained<NSWindow>,
    width: u32,
    height: u32,
    half_interval: Duration,
    loaded: Rc<Cell<bool>>,
    cleared: Rc<Cell<bool>>,
    closed: Arc<AtomicBool>,
    busy: Cell<bool>,
    reported: Cell<bool>,
    last_frame: Cell<Option<Time>>,
    pool: RefCell<FramePool>,
    sink: Arc<VideoSink<PixelBuffer>>,
}

impl Snapshotter {
    fn next_stamp(&self, raw: Time) -> Time {
        let stamped = self
            .last_frame
            .get()
            .map_or(raw, |prev| raw.max(prev + Duration::from_nanos(1)));
        self.last_frame.set(Some(stamped));
        stamped
    }

    fn tick(self: &Rc<Self>, mtm: MainThreadMarker) {
        if self.closed.load(Ordering::SeqCst) || self.busy.get() {
            return;
        }
        if !self.loaded.get() {
            if !self.cleared.replace(true) {
                let empty = self.pool.borrow_mut().draw(&mut |pixels, _| pixels.fill(0));
                if let Ok(Some(frame)) = empty {
                    (self.sink.frame)(frame, self.next_stamp(host_time()));
                }
            }
            return;
        }
        if unsafe { self.web.isLoading() } {
            return;
        }
        self.busy.set(true);
        let config = unsafe { WKSnapshotConfiguration::new(mtm) };
        // One snapshot pixel per frame pixel, whatever the window's
        // backing scale (2 on Retina screens, which costs four times
        // as much to draw and then to scale down).
        let points = f64::from(self.width) / self.window.backingScaleFactor();
        unsafe { config.setSnapshotWidth(Some(&NSNumber::new_f64(points))) };
        let captured = self.next_stamp(host_time() - self.half_interval);
        let state = self.clone();
        let done = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            state.busy.set(false);
            if state.closed.load(Ordering::SeqCst) {
                return;
            }
            let drawn = match unsafe { image.as_ref() } {
                Some(image) => draw(
                    &mut state.pool.borrow_mut(),
                    image,
                    state.width,
                    state.height,
                ),
                None => Err(match unsafe { error.as_ref() } {
                    Some(error) => anyhow!("{}", error.localizedDescription()),
                    None => anyhow!("no picture"),
                }),
            };
            match drawn {
                Ok(Some(frame)) => (state.sink.frame)(frame, captured),
                // The pool is out of frames: the consumer is behind.
                Ok(None) => {}
                Err(err) => {
                    if !state.reported.replace(true) {
                        isocast_diagnostics::diagnostic!("warn";
                            "isocastd: browser source snapshot failed: {err:#}"
                        );
                    }
                }
            }
        });
        unsafe {
            self.web
                .takeSnapshotWithConfiguration_completionHandler(Some(&config), &done)
        };
    }
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
            let wall_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|d| u64::try_from(d.as_micros()).ok());
            let body = unsafe { message.body() };
            let decoded = body
                .downcast::<NSString>()
                .map_err(|_| anyhow!("not a string"))
                .and_then(|text| decode(&text));
            match decoded {
                Ok((rate, timing, samples)) => {
                    // The document-start script sends an empty first record.
                    // Ordered with its audio, even on page-initiated reload.
                    if samples.is_empty() {
                        self.flush(now);
                        self.ivars().stamper.replace(Default::default());
                        self.ivars().resampler.replace(Default::default());
                        return;
                    }
                    let samples = self.ivars().resampler.borrow_mut().run(rate, samples);
                    let frames = samples.len() / 2;
                    if frames > 0
                        && let Some((at, gap)) = self
                            .ivars()
                            .stamper
                            .borrow_mut()
                            .stamp_timed(frames, timing, wall_us, now)
                    {
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

    fn flush(&self, now: Time) {
        let pending = self.ivars().stamper.borrow_mut().finish(now);
        if let Some((until, gap)) = pending {
            (self.ivars().sink.report_gap)(until, gap);
        }
    }
}

/// A message from `audio.js`: `RATE[(@|+)END_US]:BASE64`, the base64 of
/// interleaved stereo 32-bit floats.
fn decode(message: &NSString) -> Result<(u32, BrowserAudioTiming, Vec<f32>)> {
    // Bound the native string before copying it. RATE (u32), optional
    // separator + u64 microseconds, colon and padded base64 are ASCII.
    let max_bytes = *AUDIO_RATES.end() as usize * FRAME_BYTES;
    if message.length() > 32 + max_bytes.div_ceil(3) * 4 {
        bail!("oversized browser audio message");
    }
    let message = message.to_string();
    let (head, data) = message.split_once(':').context("no sample rate")?;
    let (rate, timing) = if let Some((rate, end_us)) = head.split_once('@') {
        (
            rate.parse().context("bad sample rate")?,
            BrowserAudioTiming::Continuous {
                end_us: end_us.parse().context("bad audio timestamp")?,
            },
        )
    } else if let Some((rate, end_us)) = head.split_once('+') {
        (
            rate.parse().context("bad sample rate")?,
            BrowserAudioTiming::Resumed {
                end_us: end_us.parse().context("bad audio timestamp")?,
            },
        )
    } else {
        (
            head.parse().context("bad sample rate")?,
            BrowserAudioTiming::Arrival,
        )
    };
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
    Ok((rate, timing, samples))
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
        let (rate, timing, decoded) = decode(&NSString::from_str(&message)).unwrap();
        assert_eq!(timing, BrowserAudioTiming::Arrival);
        assert_eq!(decoded, samples);
        let mut resampler = Resampler::default();
        let out = resampler.run(rate, decoded);
        assert_eq!(out.len(), 4);
        assert_eq!(&out[..2], &samples[..2]);
        assert!((out[2] - 0.959375).abs() < 1e-6);
        assert!((out[3] - -0.0203125).abs() < 1e-6);
        let timed = message.replacen("44100:", "48000+75000:", 1);
        let (rate, timing, decoded) = decode(&NSString::from_str(&timed)).unwrap();
        assert_eq!(timing, BrowserAudioTiming::Resumed { end_us: 75_000 });
        assert_eq!(resampler.run(rate, decoded), samples);
        assert!(decode(&NSString::from_str("48000@bad:")).is_err());
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
            assert_eq!(decode(&message(rate, second)).unwrap().2.len(), second / 4);
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

    #[test]
    fn suspend_race_keeps_resumed_tag_for_post_gap_chunk() {
        // Exercise the real audio.js + serialized worklet module when
        // suspend() sends 'pause' while pre-suspend quanta continue to arrive
        // (both a full chunk and a partial tail) before the >=30 ms gap.
        let harness = r#"
const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
globalThis.btoa = bytes => {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const a = bytes.charCodeAt(i);
    const b = i + 1 < bytes.length ? bytes.charCodeAt(i + 1) : 0;
    const c = i + 2 < bytes.length ? bytes.charCodeAt(i + 2) : 0;
    const n = (a << 16) | (b << 8) | c;
    out += chars[(n >> 18) & 63] + chars[(n >> 12) & 63]
      + (i + 1 < bytes.length ? chars[(n >> 6) & 63] : '=')
      + (i + 2 < bytes.length ? chars[n & 63] : '=');
  }
  return out;
};
let clockMs = 1_000_000;
Date.now = () => clockMs;
globalThis.sampleRate = 48000;
const sent = [];
let WorkletClass, workletInstance;
globalThis.AudioWorkletProcessor = class {
  constructor() {
    this.port = { onmessage: null, postMessage: msg => this._toMain?.(msg) };
  }
};
globalThis.registerProcessor = (_name, cls) => { WorkletClass = cls; };
class AudioNode { connect(t) { return t; } disconnect() {} }
class AudioDestinationNode extends AudioNode { constructor(ctx) { super(); this.context = ctx; } }
class FakeAudioContext {
  constructor() {
    this.state = 'running';
    this.sampleRate = 48000;
    this.baseLatency = 0.01;
    this.outputLatency = 0.04;
    this.destination = new AudioDestinationNode(this);
    this._listeners = [];
    this.audioWorklet = {
      addModule: url => {
        eval(decodeURIComponent(url.slice('data:text/javascript,'.length)));
        return Promise.resolve();
      }
    };
  }
  createGain() { const n = new AudioNode(); n.gain = { value: 1 }; return n; }
  createDelay() { const n = new AudioNode(); n.delayTime = { value: 0 }; return n; }
  createOscillator() { const n = new AudioNode(); n.start = () => {}; n.stop = () => {}; return n; }
  createMediaElementSource() { return new AudioNode(); }
  addEventListener(_e, fn) { this._listeners.push(fn); }
  suspend() { this.state = 'suspended'; this._listeners.forEach(f => f()); }
  resume() { this.state = 'running'; this._listeners.forEach(f => f()); }
}
globalThis.AudioNode = AudioNode;
globalThis.AudioDestinationNode = AudioDestinationNode;
globalThis.HTMLMediaElement = class { play() {} };
globalThis.AudioWorkletNode = class extends AudioNode {
  constructor() {
    super();
    workletInstance = new WorkletClass();
    this.port = {
      onmessage: null,
      postMessage: data => workletInstance.port.onmessage?.({ data }),
    };
    workletInstance._toMain = data => this.port.onmessage?.({ data });
  }
};
globalThis.document = { addEventListener: () => {} };
globalThis.console = { info: () => {}, warn: () => {} };
globalThis.window = globalThis;
globalThis.AudioContext = FakeAudioContext;
globalThis.webkit = { messageHandlers: { isocastAudio: { postMessage: m => { sent.push(m); } } } };
"#;
        let driver = r#"
ObjC.import('Foundation');
(async () => {
  const ctx = new AudioContext();
  const osc = new AudioNode();
  osc.connect(ctx.destination);
  await Promise.resolve();
  await Promise.resolve();
  const q = new Float32Array(128).fill(0.25);
  function step(n, dt = 3) {
    for (let i = 0; i < n; i++) {
      clockMs += dt;
      workletInstance.process([[q, q]]);
    }
  }
  // 10 quanta (1280 frames = 1 full chunk + 256 partial frames) before suspend.
  step(10);
  // suspend() sends 'pause', flushing the 256-frame partial, THEN 9 more
  // pre-suspend quanta (1 full 1024-frame chunk + 128 partial frames) still
  // run before the audio thread actually halts for 75 ms.
  ctx.suspend();
  step(9);
  // Now the audio thread halts for 75 ms, resumes, and renders 10 quanta.
  clockMs += 75;
  ctx.resume();
  workletInstance.process([[q, q]]);
  step(9);
  // Also exercise late 'pause' (posted at suspend before the 75 ms pause,
  // delivered to the worklet 1 quantum after the >=30 ms gap): still tags '+'.
  const pauseSentMs = clockMs;
  clockMs += 75;
  workletInstance.process([[q, q]]);
  workletInstance.port.onmessage({ data: { type: 'pause', at: pauseSentMs } });
  step(8);
  // Exercise an involuntary >=30 ms render hiccup 1 quantum BEFORE suspend(),
  // followed by a 75 ms suspend: pauseArmed must stay armed for the real suspend.
  clockMs += 35;
  workletInstance.process([[q, q]]);
  clockMs += 3;
  ctx.suspend();
  clockMs += 75;
  ctx.resume();
  step(8);
  const data = $(JSON.stringify(sent) + '\n').dataUsingEncoding($.NSUTF8StringEncoding);
  $.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
})();
"#;
        let script = format!("{harness}\n{}\n{driver}", include_str!("audio.js"));
        let output = std::process::Command::new("/usr/bin/osascript")
            .args(["-l", "JavaScript", "-e", &script])
            .output()
            .expect("osascript runs JavaScriptCore");
        assert!(
            output.status.success(),
            "osascript failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let first_line = std::str::from_utf8(&output.stdout)
            .expect("UTF-8")
            .lines()
            .next()
            .expect("JSON line");
        let messages: Vec<String> =
            serde_json::from_str(first_line).expect("JSON array of messages");
        let mut stamper = Stamper::default();
        let mut decoded_runs = Vec::new();
        for (i, text) in messages.iter().enumerate() {
            let (_rate, timing, samples) = decode(&NSString::from_str(text)).unwrap();
            let frames = samples.len() / 2;
            let end_us = match timing {
                BrowserAudioTiming::Continuous { end_us }
                | BrowserAudioTiming::Resumed { end_us } => end_us,
                BrowserAudioTiming::Arrival => panic!("expected timed chunk: {text}"),
            };
            // Deliver in three ~130 ms late IPC bursts (within MAX_LAG = 195 ms).
            let arrival_us = if i <= 4 {
                1_000_160_000
            } else if i <= 6 {
                1_000_260_000
            } else {
                1_000_420_000
            };
            let now = isocast_engine::Time::from_nanos(arrival_us as i64 * 1_000);
            let (at, gap) = stamper
                .stamp_timed(frames, timing, Some(arrival_us), now)
                .expect("burst within MAX_LAG retained");
            assert_eq!(gap, None, "unexpected dropout on {timing:?}");
            decoded_runs.push((
                matches!(timing, BrowserAudioTiming::Resumed { .. }),
                frames,
                at,
                end_us,
            ));
        }
        // Chunks 0..4 are pre-suspend (1024, 256, 1024, 128 frames): all Continuous,
        // with chunk 0's end_us shifted +50 ms (baseLatency 10 ms + outputLatency 40 ms).
        // Chunk 4 is post-resume #1 (Resumed), chunk 5 is its 256-frame tail, chunk 6
        // is post-resume #2 (late 'pause', Resumed), chunks 7..9 are the 128-frame tail
        // flushed at the 35 ms hiccup and the 128-frame hiccup quantum flushed on suspend
        // (both Continuous), and chunk 9 is post-resume #3 (Resumed).
        assert_eq!(decoded_runs[0].3, 1_000_074_000);
        let flags: Vec<bool> = decoded_runs.iter().map(|r| r.0).collect();
        assert_eq!(
            flags,
            [
                false, false, false, false, true, false, true, false, false, true
            ],
            "{decoded_runs:?}"
        );
        for (pre, post, expected) in [(3, 4, 70..=80), (5, 6, 70..=80), (8, 9, 105..=120)] {
            let pre_end = decoded_runs[pre]
                .2
                .add_ticks(decoded_runs[pre].1 as u64, SAMPLE_RATE);
            let hole_ms = decoded_runs[post].2.saturating_since(pre_end).as_millis();
            assert!(
                expected.contains(&hole_ms),
                "expected {expected:?} ms hole at {pre}->{post}, got {hole_ms} ms ({decoded_runs:?})"
            );
        }
    }

    #[test]
    fn webkit_version_parses_and_selects_expected_media_delay() {
        assert_eq!(parse_webkit_short_version("21624"), Some(21_624));
        assert_eq!(parse_webkit_short_version("21624.5.1.11.3"), Some(21_624));
        assert_eq!(parse_webkit_short_version("22625"), Some(22_625));
        assert_eq!(parse_webkit_short_version(" 22625.1.29 "), Some(22_625));
        assert_eq!(parse_webkit_short_version(""), None);
        assert_eq!(parse_webkit_short_version("0"), None);
        assert_eq!(parse_webkit_short_version("unknown"), None);

        let live = webkit_short_version();
        assert!(
            live.is_some_and(|v| v >= 18_000),
            "expected readable WebKit CFBundleShortVersionString on macOS, got {live:?}"
        );

        let probe_delay = |apple_webkit: bool, version_js: &str| -> Vec<f64> {
            let preamble = format!(
                r#"
ObjC.import('Foundation');
const delays = [];
class AudioNode {{ connect(t) {{ return t; }} disconnect() {{}} }}
class AudioDestinationNode extends AudioNode {{ constructor(ctx) {{ super(); this.context = ctx; }} }}
class FakeAudioContext {{
  constructor() {{
    this.state = 'running';
    this.sampleRate = 48000;
    this.baseLatency = 0.00267;
    this.outputLatency = 0.01;
    this.destination = new AudioDestinationNode(this);
    this.audioWorklet = {{ addModule: () => new Promise(() => {{}}) }};
  }}
  createGain() {{ const n = new AudioNode(); n.gain = {{ value: 1 }}; return n; }}
  createDelay() {{
    const d = {{ value: 0 }};
    delays.push(d);
    const n = new AudioNode();
    n.delayTime = d;
    return n;
  }}
  createOscillator() {{ const n = new AudioNode(); n.start = () => {{}}; n.stop = () => {{}}; return n; }}
  createMediaElementSource() {{ return new AudioNode(); }}
  addEventListener() {{}}
  suspend() {{}}
  resume() {{}}
}}
globalThis.AudioNode = AudioNode;
globalThis.AudioDestinationNode = AudioDestinationNode;
globalThis.HTMLMediaElement = class {{ play() {{}} }};
globalThis.document = {{ addEventListener: () => {{}} }};
globalThis.console = {{ info: () => {{}}, warn: () => {{}} }};
globalThis.window = globalThis;
globalThis.AudioContext = FakeAudioContext;
globalThis.webkit = {{ messageHandlers: {{ isocastAudio: {{ postMessage: () => {{}} }} }} }};
globalThis.__isocastAppleWebKit = {apple_webkit};
globalThis.__isocastAppleWebKitVersion = {version_js};
"#
            );
            let tail = r#"
if ('__isocastAppleWebKitVersion' in window) {
  throw new Error('__isocastAppleWebKitVersion was not deleted');
}
const ctx = new AudioContext();
ctx.createMediaElementSource({});
const out = JSON.stringify(delays.map(d => d.value)) + '\n';
$.NSFileHandle.fileHandleWithStandardOutput.writeData($(out).dataUsingEncoding($.NSUTF8StringEncoding));
"#;
            let script = format!("{preamble}\n{}\n{tail}", include_str!("audio.js"));
            let output = std::process::Command::new("/usr/bin/osascript")
                .args(["-l", "JavaScript", "-e", &script])
                .output()
                .expect("osascript runs JavaScriptCore");
            assert!(
                output.status.success(),
                "osascript failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(output.stdout.trim_ascii()).expect("JSON delays")
        };

        assert_eq!(probe_delay(true, "21624"), vec![0.1]);
        assert_eq!(probe_delay(true, "22624"), vec![0.1]);
        assert_eq!(probe_delay(true, "22625"), vec![0.015]);
        assert_eq!(probe_delay(true, "null"), vec![0.015]);
        assert_eq!(probe_delay(true, "undefined"), vec![0.015]);
        assert_eq!(probe_delay(false, "21624"), Vec::<f64>::new());
    }

    #[test]
    fn snapshot_schedule_aligns_to_timeline_half_slot_grid() {
        let ms = |n: i64| Time::from_nanos(n * 1_000_000);
        let mut sched = SnapshotSchedule::new(ms(100), 30);
        // Before `grid` is known, warmup ticks follow `fallback_origin` at 30 Hz.
        let d1 = sched.next_deadline(ms(100), None);
        assert_eq!(d1, ms(100).add_ticks(1, 30));
        // Once `grid` is set (including slightly in the future for START_DELAY),
        // deadlines lock to `grid.add_ticks(k, 30) + 1 / (2 * 30)` without double-ticking.
        let grid = ms(210);
        let d2 = sched.next_deadline(ms(167), Some(grid));
        assert!(d2 >= ms(175), "handoff fired too soon: {d2:?}");
        assert_eq!(d2, grid_deadline(grid, -1, 30));
        for k in 0..10i64 {
            let now = grid_deadline(grid, k - 1, 30) + Duration::from_micros(200);
            let dk = sched.next_deadline(now, Some(grid));
            let expected = grid.add_ticks(k as u64, 30) + Duration::from_nanos(500_000_000 / 30);
            assert!(
                (dk.as_nanos() - expected.as_nanos()).abs() <= 1,
                "slot {k}: got {dk:?}, expected {expected:?}"
            );
        }
        // A multi-hour stall jumps in O(1) to the next future half-slot without bursting.
        let stalled_now = grid + Duration::from_secs(8 * 3_600);
        let after_stall = sched.next_deadline(stalled_now, Some(grid));
        assert!(after_stall > stalled_now);
        assert!(after_stall <= stalled_now + Duration::from_secs(1) / 30);
    }
}
