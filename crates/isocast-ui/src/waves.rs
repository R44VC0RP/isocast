//! Waveforms for the editor's tracks (#87): each file's first audio track
//! as peaks, 100 a second, made once on a thread of their own (AVFoundation's
//! asset reader on macOS; none elsewhere) and kept in memory for as long as
//! the app runs, and on disk in the temporary folder
//! (`$TMPDIR/isocast-waveforms`, by the file's path, size and time), so a
//! file is only read once. The window's thread asks with `request` and
//! collects what is ready with `collect`, as with thumbnails; drawing never
//! waits for them.
//!
//! A waveform is drawn as pictures [`CHUNK`] pixels wide (white on
//! transparent, coloured by the interface), each covering the same stretch
//! of the file, so a long take is many small pictures rather than one too
//! wide for the GPU.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::thumbs::FileKey;

/// Peaks a second.
pub const RATE: usize = 100;
/// Each picture's width, and height, in pixels.
const CHUNK: usize = 2048;
const HEIGHT: usize = 32;
/// The most pixels a file's pictures add up to (a long take gets fewer a
/// second).
const MOST_PIXELS: usize = 60_000;

struct Job {
    key: FileKey,
}

/// A file's waveform pictures, made on the waveforms' thread (#94: not on
/// the window's), and the share of the file each covers.
struct Done {
    key: FileKey,
    buffers: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    chunk_seconds: f64,
}

/// A file's waveform, ready to draw: its pictures and the share of the
/// file each covers.
#[derive(Clone)]
pub struct Wave {
    pub images: ModelRc<Image>,
    /// Of the peaks' length (the file's audio), in seconds.
    pub chunk_seconds: f64,
}

#[derive(Default)]
pub struct Waves {
    jobs: RefCell<Option<(Sender<Job>, Receiver<Done>)>>,
    cache: RefCell<HashMap<FileKey, Wave>>,
    pending: RefCell<HashSet<FileKey>>,
}

impl Waves {
    /// The waveform of `path`, once made (no pictures if it has no sound).
    pub fn get(&self, path: &Path) -> Option<Wave> {
        self.cache.borrow().get(&FileKey::of(path)?).cloned()
    }

    /// Make the waveform of `path` unless it is made or being made. True
    /// if a new job was sent.
    pub fn request(&self, path: &Path) -> bool {
        let Some(key) = FileKey::of(path) else {
            return false;
        };
        if self.cache.borrow().contains_key(&key) || self.pending.borrow().contains(&key) {
            return false;
        }
        let mut jobs = self.jobs.borrow_mut();
        let (tx, _) = jobs.get_or_insert_with(start);
        let sent = tx.send(Job { key: key.clone() }).is_ok();
        if sent {
            self.pending.borrow_mut().insert(key);
        }
        sent
    }

    /// Take in the waveforms made since last time. True if there were any.
    pub fn collect(&self) -> bool {
        let jobs = self.jobs.borrow();
        let Some((_, rx)) = jobs.as_ref() else {
            return false;
        };
        let mut any = false;
        while let Ok(done) = rx.try_recv() {
            let images: Vec<Image> = done.buffers.into_iter().map(Image::from_rgba8).collect();
            let wave = Wave {
                images: ModelRc::new(VecModel::from(images)),
                chunk_seconds: done.chunk_seconds,
            };
            self.pending.borrow_mut().remove(&done.key);
            let mut cache = self.cache.borrow_mut();
            // The file changed under its path: its old waveform goes (#94).
            cache.retain(|key, _| key.path != done.key.path);
            cache.insert(done.key, wave);
            any = true;
        }
        any
    }

    /// Forget the waveforms of files not in `paths`.
    pub fn keep(&self, paths: &HashSet<&Path>) {
        self.cache
            .borrow_mut()
            .retain(|key, _| paths.contains(key.path.as_path()));
    }

    /// Nothing is being made.
    pub fn idle(&self) -> bool {
        self.pending.borrow().is_empty()
    }
}

/// The thread that reads them, one file at a time; it ends when `Waves`
/// is dropped.
fn start() -> (Sender<Job>, Receiver<Done>) {
    let (jobs, inbox) = channel::<Job>();
    let (outbox, done) = channel::<Done>();
    let spawned = std::thread::Builder::new()
        .name("waveforms".into())
        .spawn(move || {
            while let Ok(job) = inbox.recv() {
                let path = &job.key.path;
                // A failed read shows no sound now but isn't kept on disk,
                // so the app's next run tries again (only a file's real
                // peaks, or none for a file without sound, are kept).
                let peaks = cached(path).unwrap_or_else(|| match read(path) {
                    Some(peaks) => {
                        store(path, &peaks);
                        peaks
                    }
                    None => Vec::new(),
                });
                let (buffers, chunk_seconds) = pictures(&peaks);
                if outbox
                    .send(Done {
                        key: job.key,
                        buffers,
                        chunk_seconds,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("waveforms: couldn’t start their thread: {error}");
    }
    (jobs, done)
}

/// Where `path`'s peaks are kept: named by its path, size and time, so a
/// changed file is read again.
fn cache_path(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    // FNV-1a: stable across runs (std's hasher isn't).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let key = format!("{}\0{}\0{modified}", path.display(), meta.len());
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(
        std::env::temp_dir()
            .join("isocast-waveforms")
            .join(format!("{hash:016x}.peaks")),
    )
}

fn cached(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(cache_path(path)?).ok()
}

fn store(path: &Path, peaks: &[u8]) {
    let Some(file) = cache_path(path) else { return };
    let Some(folder) = file.parent() else { return };
    if std::fs::create_dir_all(folder).is_err() {
        return;
    }
    // Written aside and renamed, so a half-written file is never read.
    let partial = file.with_extension("partial");
    if std::fs::write(&partial, peaks).is_ok() && std::fs::rename(&partial, &file).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
}

/// Pixels a second for `seconds` of sound.
fn pixels_per_second(seconds: f64) -> f64 {
    (MOST_PIXELS as f64 / seconds.max(1.0)).min(RATE as f64)
}

/// The peaks as pictures (see the module docs), and the seconds of the
/// file each covers.
fn pictures(peaks: &[u8]) -> (Vec<SharedPixelBuffer<Rgba8Pixel>>, f64) {
    let seconds = peaks.len() as f64 / RATE as f64;
    let rate = pixels_per_second(seconds);
    let width = (seconds * rate).ceil() as usize;
    let chunks = width.div_ceil(CHUNK);
    let mut images = Vec::with_capacity(chunks);
    for chunk in 0..chunks {
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(CHUNK as u32, HEIGHT as u32);
        let pixels = buffer.make_mut_slice();
        for x in 0..CHUNK {
            let column = chunk * CHUNK + x;
            if column >= width {
                break;
            }
            // The loudest peak under this column.
            let from = (column as f64 / rate * RATE as f64) as usize;
            let to = (((column + 1) as f64 / rate * RATE as f64).ceil() as usize)
                .clamp(from + 1, peaks.len().max(from + 1));
            let peak = peaks[from.min(peaks.len().saturating_sub(1))..to.min(peaks.len())]
                .iter()
                .copied()
                .max()
                .unwrap_or(0);
            // A little louder than linear, so speech shows.
            let level = (f64::from(peak) / 255.0).powf(0.6);
            let half = level * (HEIGHT as f64 / 2.0);
            for y in 0..HEIGHT {
                let from_middle = ((y as f64 + 0.5) - HEIGHT as f64 / 2.0).abs();
                let cover = (half - from_middle + 0.5).clamp(0.0, 1.0);
                // At least a hairline, so silence shows as a line.
                let cover = if from_middle <= 0.5 {
                    cover.max(0.5)
                } else {
                    cover
                };
                if cover > 0.0 {
                    pixels[y * CHUNK + x] = Rgba8Pixel {
                        r: 255,
                        g: 255,
                        b: 255,
                        a: (cover * 255.0) as u8,
                    };
                }
            }
        }
        images.push(buffer);
    }
    (images, CHUNK as f64 / rate)
}

#[cfg(not(target_os = "macos"))]
fn read(_path: &Path) -> Option<Vec<u8>> {
    None
}

/// `path`'s first audio track as peaks, [`RATE`] a second (empty if it
/// has no sound; `None` if it can't be read).
#[cfg(target_os = "macos")]
fn read(path: &Path) -> Option<Vec<u8>> {
    objc2::rc::autoreleasepool(|_| mac::peaks(path))
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CString, c_void};
    use std::path::Path;

    use objc2::encode::{Encoding, RefEncode};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{class, msg_send};

    use super::RATE;

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

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    /// CoreAudio's AudioStreamBasicDescription.
    #[repr(C)]
    struct Asbd {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMSampleBufferGetDataBuffer(sample: *const c_void) -> *const c_void;
        fn CMSampleBufferGetFormatDescription(sample: *const c_void) -> *const c_void;
        fn CMAudioFormatDescriptionGetStreamBasicDescription(format: *const c_void) -> *const Asbd;
        fn CMBlockBufferGetDataLength(buffer: *const c_void) -> usize;
        fn CMBlockBufferCopyDataBytes(
            buffer: *const c_void,
            offset: usize,
            length: usize,
            destination: *mut c_void,
        ) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: *const c_void);
    }

    fn string(text: &str) -> Option<Retained<AnyObject>> {
        let text = CString::new(text).ok()?;
        // SAFETY: a documented class method with a valid C string.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
    }

    fn number(value: u32) -> Option<Retained<AnyObject>> {
        // SAFETY: a documented class method.
        unsafe { msg_send![class!(NSNumber), numberWithUnsignedInt: value] }
    }

    fn boolean(value: bool) -> Option<Retained<AnyObject>> {
        // SAFETY: a documented class method.
        unsafe { msg_send![class!(NSNumber), numberWithBool: Bool::new(value)] }
    }

    pub fn peaks(path: &Path) -> Option<Vec<u8>> {
        // SAFETY: AVFoundation's documented methods on valid objects; the
        // sample buffers copyNextSampleBuffer returns are +1 (released
        // here), and their bytes are copied into a buffer of their length.
        unsafe {
            let url: Option<Retained<AnyObject>> =
                msg_send![class!(NSURL), fileURLWithPath: &*string(path.to_str()?)?];
            let asset: Option<Retained<AnyObject>> = msg_send![
                class!(AVURLAsset),
                URLAssetWithURL: &*url?,
                options: std::ptr::null::<AnyObject>()
            ];
            let asset = asset?;
            // AVMediaTypeAudio.
            let tracks: Option<Retained<AnyObject>> =
                msg_send![&*asset, tracksWithMediaType: &*string("soun")?];
            let track: Option<Retained<AnyObject>> = msg_send![&*tracks?, firstObject];
            // No audio track: no sound, which is an answer (kept).
            let Some(track) = track else {
                return Some(Vec::new());
            };
            // Interleaved 32-bit float, the track's own rate and channels.
            let keys = [
                string("AVFormatIDKey")?,
                string("AVLinearPCMBitDepthKey")?,
                string("AVLinearPCMIsFloatKey")?,
                string("AVLinearPCMIsBigEndianKey")?,
                string("AVLinearPCMIsNonInterleaved")?,
            ];
            let values = [
                number(u32::from_be_bytes(*b"lpcm"))?,
                number(32)?,
                boolean(true)?,
                boolean(false)?,
                boolean(false)?,
            ];
            let key_ptrs: Vec<*const AnyObject> = keys.iter().map(|k| &**k as *const _).collect();
            let value_ptrs: Vec<*const AnyObject> =
                values.iter().map(|v| &**v as *const _).collect();
            let settings: Option<Retained<AnyObject>> = msg_send![
                class!(NSDictionary),
                dictionaryWithObjects: value_ptrs.as_ptr(),
                forKeys: key_ptrs.as_ptr(),
                count: keys.len()
            ];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let reader: Option<Retained<AnyObject>> = msg_send![
                class!(AVAssetReader),
                assetReaderWithAsset: &*asset,
                error: &mut error
            ];
            let reader = reader?;
            let output: Option<Retained<AnyObject>> = msg_send![
                class!(AVAssetReaderTrackOutput),
                assetReaderTrackOutputWithTrack: &*track,
                outputSettings: &*settings?
            ];
            let output = output?;
            let _: () = msg_send![&*output, setAlwaysCopiesSampleData: Bool::NO];
            let can: Bool = msg_send![&*reader, canAddOutput: &*output];
            if !can.as_bool() {
                return None;
            }
            let _: () = msg_send![&*reader, addOutput: &*output];
            let started: Bool = msg_send![&*reader, startReading];
            if !started.as_bool() {
                return None;
            }
            let mut peaks = Vec::new();
            let mut bucket = 0usize;
            let mut in_bucket = 0usize;
            let mut loudest = 0f32;
            let mut bytes: Vec<u8> = Vec::new();
            loop {
                let sample: *mut Sample =
                    objc2::rc::autoreleasepool(|_| msg_send![&*output, copyNextSampleBuffer]);
                if sample.is_null() {
                    break;
                }
                let sample = sample as *const c_void;
                let format = CMSampleBufferGetFormatDescription(sample);
                let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(format);
                let data = CMSampleBufferGetDataBuffer(sample);
                if !asbd.is_null() && !data.is_null() {
                    let asbd = &*asbd;
                    let channels = asbd.channels_per_frame.max(1) as usize;
                    if bucket == 0 {
                        bucket = (asbd.sample_rate as usize / RATE).max(1);
                    }
                    let length = CMBlockBufferGetDataLength(data);
                    bytes.resize(length, 0);
                    if CMBlockBufferCopyDataBytes(data, 0, length, bytes.as_mut_ptr().cast()) == 0 {
                        for frame in bytes.chunks_exact(4 * channels) {
                            for value in frame.as_chunks::<4>().0 {
                                loudest = loudest.max(f32::from_le_bytes(*value).abs());
                            }
                            in_bucket += 1;
                            if in_bucket == bucket {
                                peaks.push((loudest.min(1.0) * 255.0).round() as u8);
                                in_bucket = 0;
                                loudest = 0.0;
                            }
                        }
                    }
                }
                CFRelease(sample);
            }
            if in_bucket > 0 {
                peaks.push((loudest.min(1.0) * 255.0).round() as u8);
            }
            let status: isize = msg_send![&*reader, status];
            // AVAssetReaderStatusCompleted.
            (status == 2).then_some(peaks)
        }
    }
}
