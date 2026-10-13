//! Snapshot mode (`--snapshot OUT.png`): the interface in an off-screen
//! window, saved as a PNG once it is ready and has had `--wait-ms` to
//! settle, so agents can see it.

use std::ptr;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSWindow,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImageAlphaInfo,
    CGInterpolationQuality, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize};
use objc2_web_kit::WKSnapshotConfiguration;

use super::with_main;
use crate::options::Snapshot;

/// How long to wait for the interface to say it is ready before taking
/// the picture anyway (so a broken page still shows what is wrong).
const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Pending {
    options: Snapshot,
    /// The countdown to the picture has started.
    started: bool,
}

impl Pending {
    pub(crate) fn new(options: Snapshot) -> Self {
        Self {
            options,
            started: false,
        }
    }
}

/// A borderless window far off screen, the snapshot's size.
pub(crate) fn window(mtm: MainThreadMarker, options: &Snapshot) -> Retained<NSWindow> {
    let size = NSSize::new(options.width.into(), options.height.into());
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(-30_000.0, -30_000.0), size),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setBackgroundColor(Some(&super::background_color()));
    window.orderFrontRegardless();
    window
}

/// The interface's query string: snapshot mode and the view to open.
pub(crate) fn query(options: &Snapshot) -> String {
    let mut query = String::from("?snapshot=1");
    if let Some(route) = &options.route {
        let route: String = route
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
            .collect();
        query.push_str("&route=");
        query.push_str(&route);
    }
    query
}

/// Keep WebKit painting off screen, and take the picture even if the
/// interface never says it is ready.
pub(crate) fn arm(_mtm: MainThreadMarker) {
    if let Some(web) = with_main(|main| main.web.clone()) {
        // WebKit treats an off-screen window as hidden and stops painting;
        // this private switch (guarded) keeps it going, as browser sources
        // do.
        let occlusion = sel!(_setWindowOcclusionDetectionEnabled:);
        let responds: bool = unsafe { msg_send![&*web, respondsToSelector: occlusion] };
        if responds {
            let _: () = unsafe { msg_send![&*web, _setWindowOcclusionDetectionEnabled: false] };
        }
    }
    after(READY_TIMEOUT, || {
        let waiting = with_main(|main| main.snapshot.as_ref().is_some_and(|p| !p.started));
        if waiting == Some(true) {
            eprintln!(
                "Isocast: the interface did not say it was ready; taking the snapshot anyway"
            );
            ready();
        }
    });
}

/// The interface is ready: run `--eval`, wait `--wait-ms`, then shoot.
pub(crate) fn ready() {
    let Some(Some((eval, wait))) = with_main(|main| {
        let pending = main.snapshot.as_mut()?;
        if pending.started {
            return None;
        }
        pending.started = true;
        Some((
            pending.options.eval.clone(),
            Duration::from_millis(pending.options.wait_ms),
        ))
    }) else {
        return;
    };
    if let Some(script) = eval {
        super::web::eval(&script);
    }
    after(wait, shoot);
}

fn after(wait: Duration, work: impl FnOnce() + Send + 'static) {
    let when = DispatchTime::try_from(wait).unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(when, work);
}

fn shoot() {
    let Some((web, window, options)) = with_main(|main| {
        Some((
            main.web.clone(),
            main.window.clone(),
            main.snapshot.as_ref()?.options.clone(),
        ))
    })
    .flatten() else {
        return;
    };
    let mtm = MainThreadMarker::new().expect("on the main thread");
    let config = unsafe { WKSnapshotConfiguration::new(mtm) };
    let (width, height) = (
        (options.width * options.scale) as usize,
        (options.height * options.scale) as usize,
    );
    // The snapshot is drawn at the window's backing scale; ask for the
    // width in points that makes `width` pixels.
    let points = width as f64 / window.backingScaleFactor();
    unsafe { config.setSnapshotWidth(Some(&NSNumber::new_f64(points))) };
    let done = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        let result = match unsafe { image.as_ref() } {
            Some(image) => write_png(image, width, height, &options.path),
            None => Err(match unsafe { error.as_ref() } {
                Some(error) => error.localizedDescription().to_string(),
                None => "WebKit returned no picture".into(),
            }),
        };
        match result {
            Ok(()) => {
                println!("{}", options.path.display());
                std::process::exit(0);
            }
            Err(message) => {
                eprintln!("Isocast: could not save the snapshot: {message}");
                std::process::exit(1);
            }
        }
    });
    unsafe { web.takeSnapshotWithConfiguration_completionHandler(Some(&config), &done) };
}

/// Draw `image` at exactly `width`×`height` pixels (sRGB) and save it.
fn write_png(
    image: &NSImage,
    width: usize,
    height: usize,
    path: &std::path::Path,
) -> Result<(), String> {
    let source = unsafe { image.CGImageForProposedRect_context_hints(ptr::null_mut(), None, None) }
        .ok_or("the snapshot has no bitmap")?;
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
    let context = unsafe {
        CGBitmapContextCreate(
            ptr::null_mut(),
            width,
            height,
            8,
            0,
            space.as_deref(),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .ok_or("could not make a bitmap")?;
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::High);
    CGContext::draw_image(Some(&context), rect, Some(&source));
    let drawn = CGBitmapContextCreateImage(Some(&context)).ok_or("could not read the bitmap")?;
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &drawn);
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("could not encode the PNG")?;
    if let Some(folder) = path.parent().filter(|f| !f.as_os_str().is_empty()) {
        std::fs::create_dir_all(folder).map_err(|err| format!("{}: {err}", folder.display()))?;
    }
    std::fs::write(path, data.to_vec()).map_err(|err| format!("{}: {err}", path.display()))
}
