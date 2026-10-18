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

/// A file as it is now: its path, size and modification time, so pictures
/// made of it are made again if it changes under the same path (#94).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub path: PathBuf,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileKey {
    /// `path`'s key, if it is there.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            size: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

struct Job {
    key: FileKey,
    seconds: f64,
}

struct Done {
    key: FileKey,
    frames: Vec<SharedPixelBuffer<Rgba8Pixel>>,
}

#[derive(Default)]
pub struct Thumbs {
    jobs: RefCell<Option<(Sender<Job>, Receiver<Done>)>>,
    cache: RefCell<HashMap<FileKey, ModelRc<Image>>>,
    pending: RefCell<HashSet<FileKey>>,
}

impl Thumbs {
    /// The frames of `path`, once made (an empty list if they couldn't be).
    pub fn get(&self, path: &Path) -> Option<ModelRc<Image>> {
        self.cache.borrow().get(&FileKey::of(path)?).cloned()
    }

    /// Make the frames of `path` (`seconds` long) unless they are made or
    /// being made. True if a new job was sent.
    pub fn request(&self, path: &Path, seconds: f64) -> bool {
        let Some(key) = FileKey::of(path) else {
            return false;
        };
        if self.cache.borrow().contains_key(&key) || self.pending.borrow().contains(&key) {
            return false;
        }
        let mut jobs = self.jobs.borrow_mut();
        let (tx, _) = jobs.get_or_insert_with(start);
        let sent = tx
            .send(Job {
                key: key.clone(),
                seconds,
            })
            .is_ok();
        if sent {
            self.pending.borrow_mut().insert(key);
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
            self.pending.borrow_mut().remove(&done.key);
            let mut cache = self.cache.borrow_mut();
            // The file changed under its path: its old frames go.
            cache.retain(|key, _| key.path != done.key.path);
            cache.insert(done.key, ModelRc::new(VecModel::from(images)));
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

/// A stretch of the timeline played from one file (#94): from `start` to
/// `end` on the timeline, `file_seconds` into the file at `start` (none:
/// its file is missing, nothing is shown there).
#[cfg_attr(
    not(target_os = "macos"),
    expect(dead_code, reason = "native media decoding is not implemented yet")
)]
pub struct Segment {
    pub path: Option<PathBuf>,
    pub start: f64,
    pub end: f64,
    pub file_seconds: f64,
}

/// Frames played ahead of the clock (each is ~2 MB at 960x540).
const PLAY_AHEAD: usize = 8;

/// Playing forward (#94): the timeline's pictures decoded in order on a
/// thread of their own (AVAssetReader, scaled to the preview's size by
/// VideoToolbox), a few frames ahead, instead of an exact seek per step
/// (about 25 pictures a second before). The window's thread shows the
/// newest frame due at its clock. Dropping it stops the thread (and its
/// sound).
pub struct Playback {
    frames: Receiver<Played>,
    /// A frame read and not yet due.
    next: RefCell<Option<Played>>,
    _sound: Option<crate::sound::Sound>,
}

/// A played picture's time on the timeline, and the picture (none: its
/// file is missing or can't be read, so nothing is shown from then).
type Played = (f64, Option<SharedPixelBuffer<Rgba8Pixel>>);

impl Playback {
    pub fn start(segments: Vec<Segment>, sound: Option<crate::sound::Sound>) -> Self {
        let (tx, frames) = std::sync::mpsc::sync_channel(PLAY_AHEAD);
        let spawned = std::thread::Builder::new()
            .name("playback".into())
            .spawn(move || {
                for segment in segments {
                    #[cfg(target_os = "macos")]
                    if !mac::play(&segment, PREVIEW_SIZE, &tx) {
                        return;
                    }
                    #[cfg(not(target_os = "macos"))]
                    if tx.send((segment.start, None)).is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            eprintln!("playback: couldn’t start its thread: {error}");
        }
        Self {
            frames,
            next: RefCell::new(None),
            _sound: sound,
        }
    }

    /// Replace its sound (none: silent from now on).
    pub fn set_sound(&mut self, sound: Option<crate::sound::Sound>) {
        self._sound = sound;
    }

    /// The newest frame due at `at` (timeline seconds) not shown yet, and
    /// its time (no picture: its file is offline).
    pub fn frame_at(&self, at: f64) -> Option<(f64, Option<Image>)> {
        let mut due = None;
        let mut next = self.next.borrow_mut();
        loop {
            let frame = match next.take() {
                Some(frame) => frame,
                None => match self.frames.try_recv() {
                    Ok(frame) => frame,
                    Err(_) => break,
                },
            };
            if frame.0 > at {
                *next = Some(frame);
                break;
            }
            due = Some(frame);
        }
        due.map(|(t, pixels)| (t, pixels.map(Image::from_rgba8)))
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
                FileKey,
                Option<mac::Generator>,
            > = std::collections::HashMap::new();
            while let Ok(mut job) = inbox.recv() {
                while let Ok(newer) = inbox.try_recv() {
                    job = newer;
                }
                #[cfg(target_os = "macos")]
                let frame = objc2::rc::autoreleasepool(|_| {
                    // A file changed under its path gets a new generator
                    // (the old one, and its AVAsset, go).
                    let key = FileKey::of(&job.path)?;
                    if !generators.contains_key(&key) {
                        generators.retain(|old, _| old.path != key.path);
                    }
                    generators
                        .entry(key)
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
                let frames = frames(&job.key.path, job.seconds);
                if outbox
                    .send(Done {
                        key: job.key,
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

    /// A CMSampleBuffer, as the messages that return one type it.
    #[repr(C)]
    struct Sample {
        _opaque: [u8; 0],
    }

    // SAFETY: CMSampleBufferRef's encoding.
    unsafe impl RefEncode for Sample {
        const ENCODING_REF: Encoding =
            Encoding::Pointer(&Encoding::Struct("opaqueCMSampleBuffer", &[]));
    }

    /// CoreMedia's CMTimeRange.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CMTimeRange {
        start: CMTime,
        duration: CMTime,
    }

    // SAFETY: the layout above ("{?={?=qiIq}{?=qiIq}}").
    unsafe impl Encode for CMTimeRange {
        const ENCODING: Encoding = Encoding::Struct("?", &[CMTime::ENCODING, CMTime::ENCODING]);
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMSampleBufferGetPresentationTimeStamp(sample: *mut c_void) -> CMTime;
        fn CMSampleBufferGetImageBuffer(sample: *mut c_void) -> *mut c_void;
        fn CMTimeGetSeconds(time: CMTime) -> f64;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *const u8;
        fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
        fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
        fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: *const c_void);
    }

    /// kCVPixelFormatType_32BGRA, and kCVPixelBufferLock_ReadOnly.
    const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
    const READ_ONLY: u64 = 1;

    fn ns_string(text: &str) -> Option<Retained<AnyObject>> {
        let text = CString::new(text).ok()?;
        // SAFETY: a documented class method with a valid C string.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
    }

    /// Play `segment`'s file in order, each picture (at most `size`
    /// pixels, RGBA) sent with its time on the timeline; false once the
    /// receiver is gone (playing stopped). A file that is missing or
    /// can't be read sends "no picture" at the segment's start.
    pub fn play(
        segment: &super::Segment,
        size: (f64, f64),
        tx: &std::sync::mpsc::SyncSender<super::Played>,
    ) -> bool {
        let Some(path) = &segment.path else {
            return tx.send((segment.start, None)).is_ok();
        };
        objc2::rc::autoreleasepool(|_| {
            // SAFETY: AVFoundation's documented methods on valid objects;
            // copyNextSampleBuffer returns a +1 CMSampleBuffer (released
            // below) or NULL at the end; its pixel buffer is locked while
            // read.
            unsafe {
                let Some(reader) = open(path, segment, size) else {
                    eprintln!("playback: couldn’t read {}", path.display());
                    return tx.send((segment.start, None)).is_ok();
                };
                let (reader, output) = reader;
                let mut sent = true;
                loop {
                    let sample: *mut Sample = msg_send![&*output, copyNextSampleBuffer];
                    if sample.is_null() {
                        break;
                    }
                    let sample = sample.cast::<c_void>();
                    let pts = CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sample));
                    let at = (segment.start + pts - segment.file_seconds).max(segment.start);
                    let image = CMSampleBufferGetImageBuffer(sample);
                    let frame = (at < segment.end && !image.is_null())
                        .then(|| rgba(image))
                        .flatten();
                    CFRelease(sample);
                    if let Some(frame) = frame
                        && tx.send((at, Some(frame))).is_err()
                    {
                        sent = false;
                        break;
                    }
                    if at >= segment.end {
                        break;
                    }
                }
                let _: () = msg_send![&*reader, cancelReading];
                sent
            }
        })
    }

    /// A reader of `path`'s video over the segment, scaled to fit `size`.
    unsafe fn open(
        path: &Path,
        segment: &super::Segment,
        size: (f64, f64),
    ) -> Option<(Retained<AnyObject>, Retained<AnyObject>)> {
        // SAFETY: as in `play`.
        unsafe {
            let path = ns_string(path.to_str()?)?;
            let url: Option<Retained<AnyObject>> =
                msg_send![class!(NSURL), fileURLWithPath: &*path];
            let asset: Option<Retained<AnyObject>> = msg_send![
                class!(AVURLAsset),
                URLAssetWithURL: &*url?,
                options: std::ptr::null::<AnyObject>()
            ];
            let asset = asset?;
            let video = ns_string("vide")?;
            let tracks: Option<Retained<AnyObject>> =
                msg_send![&*asset, tracksWithMediaType: &*video];
            let track: Option<Retained<AnyObject>> = msg_send![&*tracks?, firstObject];
            let track = track?;
            let natural: CGSize = msg_send![&*track, naturalSize];
            let fit = (size.0 / natural.width.max(1.0))
                .min(size.1 / natural.height.max(1.0))
                .min(1.0);
            // Even sizes, as VideoToolbox scales to.
            let even = |v: f64| ((v * fit / 2.0).round() * 2.0).max(2.0) as i64;
            let keys = [
                ns_string("PixelFormatType")?,
                ns_string("Width")?,
                ns_string("Height")?,
            ];
            let values: [Retained<AnyObject>; 3] = [
                msg_send![class!(NSNumber), numberWithUnsignedInt: BGRA],
                msg_send![class!(NSNumber), numberWithLongLong: even(natural.width)],
                msg_send![class!(NSNumber), numberWithLongLong: even(natural.height)],
            ];
            let key_ptrs: Vec<*const AnyObject> = keys.iter().map(|k| &**k as *const _).collect();
            let value_ptrs: Vec<*const AnyObject> =
                values.iter().map(|v| &**v as *const _).collect();
            let settings: Option<Retained<AnyObject>> = msg_send![
                class!(NSDictionary),
                dictionaryWithObjects: value_ptrs.as_ptr(),
                forKeys: key_ptrs.as_ptr(),
                count: 3usize
            ];
            let output: Option<Retained<AnyObject>> = msg_send![
                class!(AVAssetReaderTrackOutput),
                assetReaderTrackOutputWithTrack: &*track,
                outputSettings: &*settings?
            ];
            let output = output?;
            let _: () = msg_send![&*output, setAlwaysCopiesSampleData: Bool::NO];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let reader: Option<Retained<AnyObject>> = msg_send![
                class!(AVAssetReader),
                assetReaderWithAsset: &*asset,
                error: &mut error
            ];
            let reader = reader?;
            let length = (segment.end - segment.start).max(0.0);
            let range = CMTimeRange {
                start: CMTimeMakeWithSeconds(segment.file_seconds.max(0.0), 600),
                duration: CMTimeMakeWithSeconds(length + 0.1, 600),
            };
            let _: () = msg_send![&*reader, setTimeRange: range];
            let can: Bool = msg_send![&*reader, canAddOutput: &*output];
            if !can.as_bool() {
                return None;
            }
            let _: () = msg_send![&*reader, addOutput: &*output];
            let started: Bool = msg_send![&*reader, startReading];
            started.as_bool().then_some((reader, output))
        }
    }

    /// A locked BGRA pixel buffer's picture as RGBA.
    unsafe fn rgba(image: *mut c_void) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        // SAFETY: `image` is a valid CVPixelBuffer, read while locked.
        unsafe {
            if CVPixelBufferLockBaseAddress(image, READ_ONLY) != 0 {
                return None;
            }
            let (w, h) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
            let stride = CVPixelBufferGetBytesPerRow(image);
            let base = CVPixelBufferGetBaseAddress(image);
            let frame = (!base.is_null() && w > 0 && h > 0 && stride >= w * 4).then(|| {
                let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
                let out = buffer.make_mut_bytes();
                for y in 0..h {
                    let row = std::slice::from_raw_parts(base.add(y * stride), w * 4);
                    let to = out[y * w * 4..(y + 1) * w * 4].as_chunks_mut::<4>().0;
                    for (to, from) in to.iter_mut().zip(row.as_chunks::<4>().0) {
                        *to = [from[2], from[1], from[0], 255];
                    }
                }
                buffer
            });
            CVPixelBufferUnlockBaseAddress(image, READ_ONLY);
            frame
        }
    }

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
