//! Thumbnails for the Create view's clips (C.2): a few small frames from
//! a take's file, evenly spaced, made on a thread of their own
//! (AVFoundation's image generator on macOS; none elsewhere) and kept in
//! memory for as long as the app runs, by file. The window's thread asks
//! with `request` and collects what is ready with `collect`.
//!
//! [`Preview`] (C.3) is the Edit view's picture at the playhead: one exact
//! frame of a file at a time, on its own thread, the newest request winning
//! so scrubbing never queues up.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

/// Frames made per file: the strip shows the one nearest each tile.
pub const FRAMES: usize = 24;
/// Their largest size in pixels (a 39x22 tile at 2x is 78x44).
const MAX_SIZE: (f64, f64) = (160.0, 90.0);

struct Job {
    path: PathBuf,
    seconds: f64,
}

struct Done {
    path: PathBuf,
    frames: Vec<SharedPixelBuffer<Rgba8Pixel>>,
}

#[derive(Default)]
pub struct Thumbs {
    jobs: RefCell<Option<(Sender<Job>, Receiver<Done>)>>,
    cache: RefCell<HashMap<PathBuf, ModelRc<Image>>>,
    pending: RefCell<HashSet<PathBuf>>,
}

impl Thumbs {
    /// The frames of `path`, once made (an empty list if they couldn't be).
    pub fn get(&self, path: &Path) -> Option<ModelRc<Image>> {
        self.cache.borrow().get(path).cloned()
    }

    /// Make the frames of `path` (`seconds` long) unless they are made or
    /// being made. True if a new job was sent.
    pub fn request(&self, path: &Path, seconds: f64) -> bool {
        if self.cache.borrow().contains_key(path) || self.pending.borrow().contains(path) {
            return false;
        }
        let mut jobs = self.jobs.borrow_mut();
        let (tx, _) = jobs.get_or_insert_with(start);
        let sent = tx
            .send(Job {
                path: path.to_path_buf(),
                seconds,
            })
            .is_ok();
        if sent {
            self.pending.borrow_mut().insert(path.to_path_buf());
        }
        sent
    }

    /// Take in the frames made since last time. True if there were any.
    pub fn collect(&self) -> bool {
        let jobs = self.jobs.borrow();
        let Some((_, rx)) = jobs.as_ref() else {
            return false;
        };
        let mut any = false;
        while let Ok(done) = rx.try_recv() {
            let images: Vec<Image> = done.frames.into_iter().map(Image::from_rgba8).collect();
            self.pending.borrow_mut().remove(&done.path);
            self.cache
                .borrow_mut()
                .insert(done.path, ModelRc::new(VecModel::from(images)));
            any = true;
        }
        any
    }

    /// Nothing is being made.
    pub fn idle(&self) -> bool {
        self.pending.borrow().is_empty()
    }
}

/// The preview's largest size in pixels.
const PREVIEW_SIZE: (f64, f64) = (960.0, 540.0);

struct PreviewJob {
    serial: u64,
    path: PathBuf,
    seconds: f64,
}

/// The Edit view's picture at the playhead (see the module docs).
#[derive(Default)]
pub struct Preview {
    jobs: RefCell<Option<PreviewChannels>>,
    serial: std::cell::Cell<u64>,
    /// The last request (its file and time), so a repeat isn't made again.
    last: RefCell<Option<(PathBuf, f64)>>,
}

impl Preview {
    /// Show `path`'s frame at `seconds` next; false if it already is.
    pub fn request(&self, path: &Path, seconds: f64) -> bool {
        let key = (path.to_path_buf(), (seconds * 1000.0).round() / 1000.0);
        if self.last.borrow().as_ref() == Some(&key) {
            return false;
        }
        let serial = self.serial.get() + 1;
        self.serial.set(serial);
        let mut jobs = self.jobs.borrow_mut();
        let (tx, _) = jobs.get_or_insert_with(start_preview);
        let sent = tx
            .send(PreviewJob {
                serial,
                path: key.0.clone(),
                seconds: key.1,
            })
            .is_ok();
        *self.last.borrow_mut() = Some(key);
        sent
    }

    /// The newest frame made since last time, if any (`Some(None)`: the
    /// newest request had no picture).
    pub fn collect(&self) -> Option<Option<Image>> {
        let jobs = self.jobs.borrow();
        let (_, rx) = jobs.as_ref()?;
        let mut newest = None;
        while let Ok((serial, frame)) = rx.try_recv() {
            if serial == self.serial.get() {
                newest = Some(frame.map(Image::from_rgba8));
            }
        }
        newest
    }

    /// Wait (up to `limit`) for the newest request's frame.
    pub fn wait(&self, limit: std::time::Duration) -> Option<Option<Image>> {
        let until = std::time::Instant::now() + limit;
        loop {
            if let Some(frame) = self.collect() {
                return Some(frame);
            }
            if std::time::Instant::now() > until {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Forget the last request (its picture is gone).
    pub fn reset(&self) {
        *self.last.borrow_mut() = None;
    }
}

/// The preview's thread: it keeps a generator per file and always makes
/// the newest request, skipping those overtaken while it worked.
type PreviewChannels = (
    Sender<PreviewJob>,
    Receiver<(u64, Option<SharedPixelBuffer<Rgba8Pixel>>)>,
);

fn start_preview() -> PreviewChannels {
    let (jobs, inbox) = channel::<PreviewJob>();
    let (outbox, done) = channel();
    let spawned = std::thread::Builder::new()
        .name("preview".into())
        .spawn(move || {
            #[cfg(target_os = "macos")]
            let mut generators: std::collections::HashMap<
                PathBuf,
                Option<mac::Generator>,
            > = std::collections::HashMap::new();
            while let Ok(mut job) = inbox.recv() {
                while let Ok(newer) = inbox.try_recv() {
                    job = newer;
                }
                #[cfg(target_os = "macos")]
                let frame = objc2::rc::autoreleasepool(|_| {
                    generators
                        .entry(job.path.clone())
                        .or_insert_with(|| mac::Generator::new(&job.path, PREVIEW_SIZE, true))
                        .as_ref()
                        .and_then(|g| g.frame(job.seconds))
                });
                #[cfg(not(target_os = "macos"))]
                let frame = {
                    let _ = (&job.path, job.seconds, PREVIEW_SIZE);
                    None
                };
                if outbox.send((job.serial, frame)).is_err() {
                    break;
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("preview: couldn’t start its thread: {error}");
    }
    (jobs, done)
}

/// The thread that makes them, one file at a time; it ends when `Thumbs`
/// is dropped.
fn start() -> (Sender<Job>, Receiver<Done>) {
    let (jobs, inbox) = channel::<Job>();
    let (outbox, done) = channel::<Done>();
    let spawned = std::thread::Builder::new()
        .name("thumbnails".into())
        .spawn(move || {
            while let Ok(job) = inbox.recv() {
                let frames = frames(&job.path, job.seconds);
                if outbox
                    .send(Done {
                        path: job.path,
                        frames,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("thumbnails: couldn’t start their thread: {error}");
    }
    (jobs, done)
}

/// The times of the frames: the middle of each of `FRAMES` equal parts.
fn times(seconds: f64) -> impl Iterator<Item = f64> {
    let seconds = seconds.max(0.0);
    (0..FRAMES).map(move |i| (i as f64 + 0.5) / FRAMES as f64 * seconds)
}

#[cfg(not(target_os = "macos"))]
fn frames(_path: &Path, seconds: f64) -> Vec<SharedPixelBuffer<Rgba8Pixel>> {
    let _ = (times(seconds), MAX_SIZE);
    Vec::new()
}

#[cfg(target_os = "macos")]
fn frames(path: &Path, seconds: f64) -> Vec<SharedPixelBuffer<Rgba8Pixel>> {
    objc2::rc::autoreleasepool(|_| match mac::Generator::new(path, MAX_SIZE, false) {
        Some(generator) => times(seconds).filter_map(|t| generator.frame(t)).collect(),
        None => Vec::new(),
    })
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CString, c_void};
    use std::path::Path;
    use std::ptr::NonNull;

    use objc2::encode::{Encode, Encoding, RefEncode};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};
    use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{CGColorSpace, CGContext, CGImage};
    use slint::{Rgba8Pixel, SharedPixelBuffer};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    /// CoreMedia's CMTime.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    // SAFETY: the layout above, which is CMTime's ("{?=qiIq}").
    unsafe impl Encode for CMTime {
        const ENCODING: Encoding = Encoding::Struct(
            "?",
            &[i64::ENCODING, i32::ENCODING, u32::ENCODING, i64::ENCODING],
        );
    }
    // SAFETY: a pointer to the struct above.
    unsafe impl RefEncode for CMTime {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMTimeMakeWithSeconds(seconds: f64, preferred_timescale: i32) -> CMTime;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *const CGColorSpace,
            bitmap_info: u32,
        ) -> Option<NonNull<CGContext>>;
    }

    /// kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big: RGBA bytes.
    const RGBA: u32 = 1 | (4 << 12);

    pub struct Generator {
        generator: Retained<AnyObject>,
    }

    impl Generator {
        /// A generator of frames at most `size` pixels, the exact frame at
        /// a time if `exact` (else the nearest keyframe, quicker).
        pub fn new(path: &Path, size: (f64, f64), exact: bool) -> Option<Self> {
            let path = CString::new(path.to_str()?).ok()?;
            // SAFETY: AVFoundation's documented class methods, with valid
            // objects; each returns an autoreleased object (retained here)
            // or nil, which `Retained` turns into None.
            unsafe {
                let string: Option<Retained<AnyObject>> =
                    msg_send![class!(NSString), stringWithUTF8String: path.as_ptr()];
                let url: Option<Retained<AnyObject>> =
                    msg_send![class!(NSURL), fileURLWithPath: &*string?];
                let asset: Option<Retained<AnyObject>> = msg_send![
                    class!(AVURLAsset),
                    URLAssetWithURL: &*url?,
                    options: std::ptr::null::<AnyObject>()
                ];
                let generator: Option<Retained<AnyObject>> = msg_send![
                    class!(AVAssetImageGenerator),
                    assetImageGeneratorWithAsset: &*asset?
                ];
                let generator = generator?;
                let _: () = msg_send![&*generator, setAppliesPreferredTrackTransform: Bool::YES];
                let _: () = msg_send![
                    &*generator,
                    setMaximumSize: CGSize::new(size.0, size.1)
                ];
                if exact {
                    let zero = CMTime {
                        value: 0,
                        timescale: 1,
                        flags: 1,
                        epoch: 0,
                    };
                    let _: () = msg_send![&*generator, setRequestedTimeToleranceBefore: zero];
                    let _: () = msg_send![&*generator, setRequestedTimeToleranceAfter: zero];
                }
                Some(Self { generator })
            }
        }

        /// The frame at `seconds` (or the nearest keyframe, quick and
        /// close enough for a 39px tile, unless made exact), as RGBA.
        pub fn frame(&self, seconds: f64) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
            // SAFETY: copyCGImageAtTime:actualTime:error: returns a +1
            // CGImage (the Create Rule) or NULL with an error; the bitmap
            // context is made over a buffer that outlives it, with the
            // matching row size.
            unsafe {
                let time = CMTimeMakeWithSeconds(seconds, 600);
                let mut error: *mut AnyObject = std::ptr::null_mut();
                let image: *mut CGImage = msg_send![
                    &*self.generator,
                    copyCGImageAtTime: time,
                    actualTime: std::ptr::null_mut::<CMTime>(),
                    error: &mut error
                ];
                let image = CFRetained::from_raw(NonNull::new(image)?);
                let (w, h) = (CGImage::width(Some(&image)), CGImage::height(Some(&image)));
                if w == 0 || h == 0 {
                    return None;
                }
                let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
                let space = CGColorSpace::new_device_rgb()?;
                let context = CGBitmapContextCreate(
                    buffer.make_mut_bytes().as_mut_ptr().cast(),
                    w,
                    h,
                    8,
                    w * 4,
                    &*space,
                    RGBA,
                )?;
                let context = CFRetained::from_raw(context);
                CGContext::draw_image(
                    Some(&context),
                    CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)),
                    Some(&image),
                );
                drop(context);
                Some(buffer)
            }
        }
    }
}
