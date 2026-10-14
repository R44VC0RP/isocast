//! Thumbnails for the Create view's clips (C.2): a few small frames from
//! a take's program file, evenly spaced, made on a thread of their own
//! (AVFoundation's image generator on macOS; none elsewhere) and kept in
//! memory for as long as the app runs, by file. The window's thread asks
//! with `request` and collects what is ready with `collect`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

/// Frames made per file: the strip shows the one nearest each tile.
pub const FRAMES: usize = 8;
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
    objc2::rc::autoreleasepool(|_| match mac::Generator::new(path) {
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

    use super::MAX_SIZE;

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
        pub fn new(path: &Path) -> Option<Self> {
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
                    setMaximumSize: CGSize::new(MAX_SIZE.0, MAX_SIZE.1)
                ];
                Some(Self { generator })
            }
        }

        /// The frame nearest `seconds` (the nearest keyframe: quick, and
        /// close enough for a 39px tile), as RGBA.
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
