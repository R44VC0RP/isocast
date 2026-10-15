//! Decoding video and audio files with AVAssetReader: hardware-decoded
//! IOSurface frames (BGRA for video with alpha), and stereo float audio at
//! the session sample rate. Matroska and WebM files, which AVFoundation
//! can't open, go to `webm.rs` instead (recognised by their first bytes).

use std::mem::ManuallyDrop;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{MediaInfo, MediaPart};
use objc2::AllocAnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack, AVMediaTypeAudio,
    AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::CFString;
use objc2_core_media::{
    CMFormatDescription, CMTime, kCMFormatDescriptionExtension_ContainsAlphaChannel,
    kCMFormatDescriptionExtension_Depth,
};
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

use crate::audio::{interleaved_f32, pcm_settings};
use crate::frames::PixelBuffer;
use crate::writer::catch;

/// A file being decoded: by AVFoundation, or (Matroska and WebM) by our
/// demuxer and VideoToolbox.
pub enum MediaReader {
    AvFoundation(AvReader),
    Matroska(crate::webm::Reader),
}

pub struct AvReader {
    // Released in `drop`, inside its autorelease pool.
    reader: ManuallyDrop<Retained<AVAssetReader>>,
    output: ManuallyDrop<Retained<AVAssetReaderTrackOutput>>,
    path: String,
}

// SAFETY: the reader and its output are used by one thread at a time.
unsafe impl Send for AvReader {}

fn asset(path: &Path) -> Retained<AVURLAsset> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) }
}

/// The asset's first track of `part`.
#[allow(deprecated)] // The synchronous track list is fine off the main thread.
fn track(asset: &AVURLAsset, part: MediaPart) -> Option<Retained<AVAssetTrack>> {
    let media_type = unsafe {
        match part {
            MediaPart::Video => AVMediaTypeVideo,
            MediaPart::Audio => AVMediaTypeAudio,
        }
    }?;
    let tracks: Retained<NSArray<AVAssetTrack>> = unsafe { asset.tracksWithMediaType(media_type) };
    tracks.firstObject()
}

/// Whether a video `track` has an alpha channel, like ProRes 4444 with
/// alpha or HEVC with alpha: its format says so, or has 32 bits a pixel.
fn has_alpha(track: &AVAssetTrack) -> bool {
    let descriptions = unsafe { track.formatDescriptions() };
    descriptions.iter().any(|description| {
        // SAFETY: a track's format descriptions are CMFormatDescriptions.
        let description =
            unsafe { &*(Retained::as_ptr(&description) as *const CMFormatDescription) };
        // SAFETY: CFNumber and CFBoolean are toll-free bridged with NSNumber.
        let number = |key: &CFString| {
            unsafe { description.extension(key) }
                .map(|value| unsafe { &*(&*value as *const _ as *const NSNumber) }.as_i64())
        };
        unsafe {
            number(kCMFormatDescriptionExtension_ContainsAlphaChannel) == Some(1)
                || number(kCMFormatDescriptionExtension_Depth) == Some(32)
        }
    })
}

fn seconds(time: CMTime) -> f64 {
    if time.timescale == 0 {
        0.0
    } else {
        time.value as f64 / f64::from(time.timescale)
    }
}

pub fn media_info(path: &Path) -> Result<MediaInfo> {
    if crate::matroska::sniff(path) {
        return autoreleasepool(|_| crate::webm::media_info(path));
    }
    // The engine's threads have no autorelease pool of their own; without
    // one, what AVFoundation autoreleases here would stay until the thread
    // ends.
    autoreleasepool(|_| read_media_info(path))
}

fn read_media_info(path: &Path) -> Result<MediaInfo> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let asset = asset(path);
    let video = track(&asset, MediaPart::Video).map(|track| {
        let size = unsafe { track.naturalSize() };
        (size.width.round() as u32, size.height.round() as u32)
    });
    let audio = track(&asset, MediaPart::Audio).is_some();
    if video.is_none() && !audio {
        bail!("{} has no video or audio macOS can play", path.display());
    }
    let duration = seconds(unsafe { asset.duration() }).max(0.0);
    Ok(MediaInfo {
        video,
        audio,
        duration: Duration::from_secs_f64(duration),
    })
}

/// Open `path` to read `part`. Media threads live as long as their
/// source and have no autorelease pool, so each call drains one of its
/// own: otherwise every reader opened (one per play) would stay alive, with
/// its decoding threads, until the source closes.
pub fn open_media(path: &Path, part: MediaPart) -> Result<MediaReader> {
    if crate::matroska::sniff(path) {
        return autoreleasepool(|_| crate::webm::open(path, part)).map(MediaReader::Matroska);
    }
    autoreleasepool(|_| open_reader(path, part)).map(MediaReader::AvFoundation)
}

fn open_reader(path: &Path, part: MediaPart) -> Result<AvReader> {
    let shown = path.display().to_string();
    // A missing file would otherwise read as one without that part.
    if !path.exists() {
        bail!("{shown} does not exist");
    }
    let asset = asset(path);
    let track = track(&asset, part).with_context(|| {
        format!(
            "{shown} has no {}",
            if part == MediaPart::Video {
                "video"
            } else {
                "audio"
            }
        )
    })?;
    let settings: Retained<NSDictionary<NSString, AnyObject>> = match part {
        MediaPart::Video => {
            // 4:2:0 in IOSurfaces the GPU can read, like capture frames;
            // BGRA if the video has alpha, to keep it.
            // SAFETY: CFString is toll-free bridged with NSString.
            let key = |key: &'static CFString| -> &'static NSString {
                unsafe { &*(key as *const CFString as *const NSString) }
            };
            let keys = unsafe {
                [
                    key(kCVPixelBufferPixelFormatTypeKey),
                    key(kCVPixelBufferIOSurfacePropertiesKey),
                    key(kCVPixelBufferMetalCompatibilityKey),
                ]
            };
            let format = NSNumber::new_u32(match has_alpha(&track) {
                true => kCVPixelFormatType_32BGRA,
                false => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            });
            let io_surface = NSDictionary::<NSString, AnyObject>::new();
            let metal = NSNumber::new_bool(true);
            let values: [&AnyObject; 3] = [&format, &io_surface, &metal];
            NSDictionary::from_slices(&keys, &values)
        }
        MediaPart::Audio => pcm_settings(2),
    };
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            &track,
            Some(&settings),
        )
    };
    unsafe { output.setAlwaysCopiesSampleData(false) };
    let reader = unsafe { AVAssetReader::initWithAsset_error(AVAssetReader::alloc(), &asset) }
        .map_err(|err| anyhow!("could not read {shown}: {}", err.localizedDescription()))?;
    let started = catch(|| unsafe {
        reader.addOutput(&output);
        reader.startReading()
    })?;
    if !started {
        let reason = unsafe { reader.error() }.map_or_else(
            || "unknown reason".into(),
            |e| e.localizedDescription().to_string(),
        );
        bail!("could not read {shown}: {reason}");
    }
    Ok(AvReader {
        reader: ManuallyDrop::new(reader),
        output: ManuallyDrop::new(output),
        path: shown,
    })
}

impl AvReader {
    /// The next sample buffer, `None` at the end.
    fn next(&mut self) -> Result<Option<Retained<objc2_core_media::CMSampleBuffer>>> {
        autoreleasepool(|_| self.read_next())
    }

    fn read_next(&mut self) -> Result<Option<Retained<objc2_core_media::CMSampleBuffer>>> {
        let sample = catch(|| unsafe { self.output.copyNextSampleBuffer() })?;
        if sample.is_none() && unsafe { self.reader.status() } == AVAssetReaderStatus::Failed {
            let reason = unsafe { self.reader.error() }.map_or_else(
                || "unknown reason".into(),
                |e| e.localizedDescription().to_string(),
            );
            bail!("could not read {}: {reason}", self.path);
        }
        Ok(sample)
    }
}

impl Drop for AvReader {
    fn drop(&mut self) {
        // A reader dropped part way (its source went off air) stops its
        // decoding now rather than whenever it is deallocated. Releasing
        // the reader and its output autoreleases AVFoundation objects (an
        // NSError, notification listeners) on this long-lived media
        // thread, so they are released in the pool too: otherwise each
        // loop of a looping source kept them until the source closed.
        // SAFETY: the reader and output are not used after this.
        autoreleasepool(|_| unsafe {
            self.reader.cancelReading();
            ManuallyDrop::drop(&mut self.output);
            ManuallyDrop::drop(&mut self.reader);
        });
    }
}

impl isocast_engine::MediaReader for MediaReader {
    type Frame = PixelBuffer;

    fn next_video(&mut self) -> Result<Option<(PixelBuffer, Duration)>> {
        match self {
            MediaReader::AvFoundation(reader) => reader.next_video(),
            // Its decoders autorelease too, on this long-lived thread.
            MediaReader::Matroska(reader) => {
                autoreleasepool(|_| isocast_engine::MediaReader::next_video(reader))
            }
        }
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        match self {
            MediaReader::AvFoundation(reader) => reader.next_audio(),
            MediaReader::Matroska(reader) => {
                autoreleasepool(|_| isocast_engine::MediaReader::next_audio(reader))
            }
        }
    }
}

impl AvReader {
    fn next_video(&mut self) -> Result<Option<(PixelBuffer, Duration)>> {
        while let Some(sample) = self.next()? {
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                continue;
            };
            let at = seconds(unsafe { sample.presentation_time_stamp() }).max(0.0);
            return Ok(Some((PixelBuffer(image), Duration::from_secs_f64(at))));
        }
        Ok(None)
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        while let Some(sample) = self.next()? {
            let Some(samples) = interleaved_f32(&sample, 2)? else {
                continue;
            };
            let at = seconds(unsafe { sample.presentation_time_stamp() }).max(0.0);
            return Ok(Some((samples, Duration::from_secs_f64(at))));
        }
        Ok(None)
    }
}
