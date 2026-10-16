//! Matroska and WebM media, which AVFoundation can't open: our demuxer
//! (`matroska.rs`), video decoded by VideoToolbox (VP9, with its alpha
//! channel from BlockAdditional; AV1 where the Mac decodes it; H.264 and
//! HEVC) or by libvpx (VP8 and its alpha, `vp8.rs`), and Opus or AAC audio
//! decoded by AudioToolbox, Vorbis by lewton. Frames come out
//! like the AVFoundation path's: IOSurface pixel buffers, 4:2:0 or (with
//! alpha) BGRA with straight alpha, and stereo float audio at 48 kHz.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, Once};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{MediaInfo, MediaPart, SAMPLE_RATE};
use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef,
    AudioConverterSetProperty, kAudioConverterDecompressionMagicCookie,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
    kAudioFormatOpus,
};
use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType, kCFAllocatorNull,
};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoCodecType, CMVideoFormatDescriptionCreate, kCMFormatDescriptionExtension_FullRangeVideo,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMTimeInvalid,
    kCMVideoCodecType_AV1, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, kCMVideoCodecType_VP9,
};
use objc2_core_video::{
    CVAttachmentMode, CVImageBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferColorPrimaries_SMPTE_C, kCVImageBufferColorPrimariesKey,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferTransferFunctionKey,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrix_SMPTE_240M_1995,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTIsHardwareDecodeSupported,
    VTRegisterSupplementalVideoDecoderIfAvailable,
};

use crate::frames::{FramePool, PixelBuffer};
use crate::matroska::{Colour, Demuxer, MAX_DURATION_NS, Packet, Track, TrackKind};
use crate::vp8::{Vp8Video, take_waiting};

/// Decoded frames held back to put H.264 and HEVC (whose frames are
/// stored in decoding order) into presentation order.
const REORDER_DEPTH: usize = 4;
/// Audio frames (at 48 kHz) per chunk: about 21 ms, so monitoring stays
/// close to the picture.
const AUDIO_CHUNK: u32 = 1024;
/// The input callback's status when the file can't be read.
const READ_FAILED: i32 = 0x72656164; // 'read'

/// Information about a Matroska/WebM file, checking that macOS can decode
/// its video (an error naming the codec if not). Audio macOS can't decode
/// is left out, with a warning in the log: the video plays without it.
pub fn media_info(path: &Path) -> Result<MediaInfo> {
    let shown = path.display().to_string();
    let mut demuxer = Demuxer::open(path).with_context(|| format!("could not read {shown}"))?;
    let container = container(&demuxer);
    let video = match demuxer.track(TrackKind::Video).cloned() {
        Some(track) => {
            // Decoding the first frame proves the codec plays here.
            VideoReader::new(&mut demuxer, &track, &shown, container)?;
            Some((track.width, track.height))
        }
        None => None,
    };
    let audio = match demuxer.track(TrackKind::Audio).cloned() {
        Some(track) => {
            let mut demuxer = Demuxer::open(path)?;
            match AudioReader::new(&mut demuxer, &track, &shown, container) {
                Ok(_) => true,
                Err(err) if video.is_some() => {
                    eprintln!("isocastd media: {err:#}; the video plays without sound");
                    false
                }
                Err(err) => return Err(err),
            }
        }
        None => false,
    };
    if video.is_none() && !audio {
        bail!("{shown} has no video or audio track");
    }
    Ok(MediaInfo {
        video,
        audio,
        duration: Duration::from_nanos(media_duration(path, &demuxer)?),
    })
}

/// How long a file plays, in nanoseconds: to the end of its last video
/// frame, unless the file says it lasts noticeably longer (sound going on
/// after the picture). Matroska's stated duration includes an audio
/// encoder's padding, which would hold the last frame and leave a gap
/// each time a looping file starts over.
fn media_duration(path: &Path, demuxer: &Demuxer) -> Result<u64> {
    let stated = demuxer.duration;
    if let Some(track) = demuxer.track(TrackKind::Video) {
        let mut scan = Demuxer::open(path)?;
        // Damaged Cues aren't worth refusing a file whose frames read.
        if let Some((last, gap)) = scan.last_frame(track.number).ok().flatten() {
            let step = track.default_duration.map_or(gap, |ns| ns as i64);
            let video = whole_ticks(last.saturating_add(step), demuxer.tick());
            return Ok(match stated {
                Some(stated) if stated > video + 100_000_000 => stated,
                _ => video,
            });
        }
    }
    match stated {
        Some(ns) => Ok(ns),
        None => scan_duration(path),
    }
}

/// A file's end time in nanoseconds, to the nearest tick: block times are
/// whole ticks (usually milliseconds), so the end is too: 2983 ms plus a
/// 16.67 ms frame is the 3000 ms it was.
fn whole_ticks(ns: i64, tick: u64) -> u64 {
    // At most the longest duration believed, so a damaged file's absurd
    // times can't make a loop wait for years.
    ((ns.max(0) as u64 + tick / 2) / tick * tick).min(MAX_DURATION_NS)
}

/// How long a file without a stated duration (a live recording) lasts:
/// its last frame's time plus one frame.
fn scan_duration(path: &Path) -> Result<u64> {
    let mut demuxer = Demuxer::open(path)?;
    let Some(track) = demuxer
        .track(TrackKind::Video)
        .or_else(|| demuxer.track(TrackKind::Audio))
        .cloned()
    else {
        return Ok(0);
    };
    demuxer.times_only = true;
    // The two latest frame times (frames may be stored out of order).
    let (mut last, mut before) = (None::<i64>, None::<i64>);
    while let Some(packet) = demuxer.next_packet(track.number)? {
        let time = Some(packet.time);
        if time > last {
            (before, last) = (last, time);
        } else if time > before && time != last {
            before = time;
        }
    }
    let Some(last) = last else {
        return Ok(0);
    };
    let step = track
        .default_duration
        .map_or(last.saturating_sub(before.unwrap_or(last)), |ns| ns as i64);
    Ok(whole_ticks(last.saturating_add(step), demuxer.tick()))
}

fn container(demuxer: &Demuxer) -> &'static str {
    if demuxer.doc_type == "webm" {
        "WebM"
    } else {
        "Matroska (MKV)"
    }
}

/// A Matroska/WebM file's video or audio, one frame or chunk at a time.
pub enum Reader {
    Video(Box<VideoReader>),
    Audio(Box<AudioReader>),
}

pub fn open(path: &Path, part: MediaPart) -> Result<Reader> {
    let shown = path.display().to_string();
    let mut demuxer = Demuxer::open(path).with_context(|| format!("could not read {shown}"))?;
    let container = container(&demuxer);
    let kind = match part {
        MediaPart::Video => TrackKind::Video,
        MediaPart::Audio => TrackKind::Audio,
    };
    let track = demuxer.track(kind).cloned().with_context(|| {
        format!(
            "{shown} has no {}",
            if part == MediaPart::Video {
                "video"
            } else {
                "audio"
            }
        )
    })?;
    Ok(match part {
        MediaPart::Video => {
            let mut reader = VideoReader::new(&mut demuxer, &track, &shown, container)?;
            reader.demuxer = Some(demuxer);
            Reader::Video(Box::new(reader))
        }
        MediaPart::Audio => {
            let mut reader = AudioReader::new(&mut demuxer, &track, &shown, container)?;
            reader.end = Some(media_duration(path, &demuxer)?.min(i64::MAX as u64) as i64);
            reader.demuxer = Some(demuxer);
            Reader::Audio(Box::new(reader))
        }
    })
}

impl isocast_engine::MediaReader for Reader {
    type Frame = PixelBuffer;

    fn next_video(&mut self) -> Result<Option<(PixelBuffer, Duration)>> {
        match self {
            Reader::Video(reader) => reader.next(),
            Reader::Audio(_) => Ok(None),
        }
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        match self {
            Reader::Audio(reader) => reader.next(),
            Reader::Video(_) => Ok(None),
        }
    }
}

fn video_codec_name(codec: &str) -> &str {
    match codec {
        "V_VP8" => "VP8",
        "V_VP9" => "VP9",
        "V_AV1" => "AV1",
        "V_MPEG4/ISO/AVC" => "H.264",
        "V_MPEGH/ISO/HEVC" => "HEVC",
        "V_MPEG2" => "MPEG-2",
        "V_THEORA" => "Theora",
        "V_PRORES" => "ProRes",
        "V_FFV1" => "FFV1",
        codec if codec.starts_with("V_MPEG4/ISO/") => "MPEG-4 Part 2",
        codec => codec,
    }
}

fn audio_codec_name(codec: &str) -> &str {
    match codec {
        "A_OPUS" => "Opus",
        "A_VORBIS" => "Vorbis",
        "A_FLAC" => "FLAC",
        "A_AC3" => "AC-3",
        "A_EAC3" => "E-AC-3",
        "A_DTS" => "DTS",
        "A_MPEG/L3" => "MP3",
        "A_MPEG/L2" => "MP2",
        codec if codec.starts_with("A_AAC") => "AAC",
        codec if codec.starts_with("A_PCM") => "PCM",
        codec => codec,
    }
}

/// "convert it: ffmpeg …" for a video macOS can't decode, keeping its
/// alpha channel if it has one.
fn conversion_hint(alpha: bool, codec: &str) -> String {
    if alpha && codec == "V_VP8" {
        // libvpx is the decoder that reads VP8's alpha.
        "convert it to VP9, which keeps the transparency: ffmpeg -c:v libvpx -i IN \
         -c:v libvpx-vp9 -pix_fmt yuva420p -c:a libopus OUT.webm"
            .into()
    } else if alpha {
        let decoder = match codec {
            "V_VP9" => "-c:v libvpx-vp9 ",
            _ => "",
        };
        format!(
            "convert it to ProRes 4444, which keeps the transparency: ffmpeg {decoder}-i IN \
             -c:v prores_ks -profile:v 4444 -pix_fmt yuva444p10le -c:a pcm_s16le OUT.mov"
        )
    } else {
        "convert it: ffmpeg -i IN -c:v libx264 -pix_fmt yuv420p -c:a aac OUT.mp4".into()
    }
}

// Video.

/// What a VP9 keyframe's uncompressed header says about the stream.
#[derive(Debug, Clone, Copy)]
struct Vp9Header {
    profile: u8,
    bit_depth: u8,
    /// VP9's color_space: 1 BT.601, 2 BT.709, 3 SMPTE 170, 4 SMPTE 240,
    /// 5 BT.2020, 7 sRGB; 0 unknown.
    color_space: u8,
    full_range: bool,
    subsampling_x: bool,
    subsampling_y: bool,
}

/// Read bits most significant first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self.data.get(self.at / 8)?;
            let bit = (byte >> (7 - self.at % 8)) & 1;
            value = (value << 1) | u32::from(bit);
            self.at += 1;
        }
        Some(value)
    }
}

/// The header of a VP9 keyframe; `None` if `data` doesn't start with one.
fn vp9_header(data: &[u8]) -> Option<Vp9Header> {
    let mut bits = Bits { data, at: 0 };
    if bits.read(2)? != 2 {
        return None;
    }
    let low = bits.read(1)?;
    let high = bits.read(1)?;
    let profile = ((high << 1) | low) as u8;
    if profile == 3 {
        bits.read(1)?;
    }
    if bits.read(1)? == 1 {
        return None; // show_existing_frame
    }
    if bits.read(1)? != 0 {
        return None; // not a keyframe
    }
    bits.read(2)?; // show_frame, error_resilient_mode
    if bits.read(24)? != 0x49_8342 {
        return None;
    }
    let bit_depth = if profile >= 2 {
        if bits.read(1)? == 1 { 12 } else { 10 }
    } else {
        8
    };
    let color_space = bits.read(3)? as u8;
    let (full_range, subsampling_x, subsampling_y);
    if color_space != 7 {
        full_range = bits.read(1)? == 1;
        if profile == 1 || profile == 3 {
            subsampling_x = bits.read(1)? == 1;
            subsampling_y = bits.read(1)? == 1;
            bits.read(1)?;
        } else {
            (subsampling_x, subsampling_y) = (true, true);
        }
    } else {
        full_range = true;
        (subsampling_x, subsampling_y) = (false, false);
        if profile == 1 || profile == 3 {
            bits.read(1)?;
        }
    }
    Some(Vp9Header {
        profile,
        bit_depth,
        color_space,
        full_range,
        subsampling_x,
        subsampling_y,
    })
}

/// A VP9 stream's `vpcC` box contents (version 1), from its keyframe
/// header and the track's colour description.
fn vpcc(header: &Vp9Header, colour: &Colour) -> Vec<u8> {
    let chroma = match (header.subsampling_x, header.subsampling_y) {
        (true, true) => 1, // 4:2:0, colocated with luma (libvpx's)
        (true, false) => 2,
        _ => 3,
    };
    let (primaries, transfer, matrix) = vp9_colour_codes(header);
    vec![
        1,
        0,
        0,
        0,
        header.profile,
        // Level: unknown, which VideoToolbox accepts.
        0,
        (header.bit_depth << 4) | (chroma << 1) | u8::from(header.full_range),
        colour.primaries.unwrap_or(primaries) as u8,
        colour.transfer.unwrap_or(transfer) as u8,
        colour.matrix.unwrap_or(matrix) as u8,
        0,
        0,
    ]
}

/// ISO/IEC 23091-4 primaries, transfer and matrix codes for a VP9
/// stream's color_space (2, unspecified, where it doesn't say).
fn vp9_colour_codes(header: &Vp9Header) -> (u64, u64, u64) {
    match header.color_space {
        1 | 3 => (6, 6, 6),
        2 => (1, 1, 1),
        4 => (7, 7, 7),
        5 => (9, 14, 9),
        7 => (1, 13, 0),
        _ => (2, 2, 2),
    }
}

/// The CoreVideo colour tags for a stream: the track's colour description,
/// else what the codec said, else Rec. 709 (the engine's assumption for
/// untagged video).
struct Tags {
    matrix: &'static CFString,
    primaries: &'static CFString,
    transfer: &'static CFString,
}

fn tags(matrix: Option<u64>, primaries: Option<u64>, transfer: Option<u64>) -> Tags {
    unsafe {
        Tags {
            matrix: match matrix {
                Some(5 | 6) => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                Some(7) => kCVImageBufferYCbCrMatrix_SMPTE_240M_1995,
                Some(9 | 10) => kCVImageBufferYCbCrMatrix_ITU_R_2020,
                _ => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            },
            primaries: match primaries {
                Some(5 | 6) => kCVImageBufferColorPrimaries_SMPTE_C,
                Some(9) => kCVImageBufferColorPrimaries_ITU_R_2020,
                _ => kCVImageBufferColorPrimaries_ITU_R_709_2,
            },
            transfer: match transfer {
                Some(16) => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                Some(18) => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
                _ => kCVImageBufferTransferFunction_ITU_R_709_2,
            },
        }
    }
}

/// A codec code the colour description gives, else the codec's own, with
/// "unspecified" (2) as nothing.
fn pick(track: Option<u64>, codec: u64) -> Option<u64> {
    track.or((codec != 2).then_some(codec))
}

/// What VideoToolbox decoded so far, filled by its output callback.
#[derive(Default)]
struct Decoded {
    frames: VecDeque<(CFRetained<CVImageBuffer>, bool)>,
    error: Option<i32>,
}

/// One VTDecompressionSession and the format of what it decodes.
struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    // Boxed so the callback's pointer to it stays put.
    decoded: Box<Mutex<Decoded>>,
}

// SAFETY: a decompression session may be used from any thread, one at a
// time; the reader is used by one thread at a time.
unsafe impl Send for Decoder {}

unsafe extern "C-unwind" fn on_decoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the refcon is the decoder's boxed `Decoded`, which outlives
    // its session.
    let decoded = unsafe { &*(refcon as *const Mutex<Decoded>) };
    let mut decoded = decoded.lock().unwrap();
    if status != 0 {
        decoded.error = Some(status);
        return;
    }
    if let Some(image) = NonNull::new(image) {
        let modifiable = flags.contains(VTDecodeInfoFlags::ImageBufferModifiable);
        decoded
            .frames
            .push_back((unsafe { CFRetained::retain(image) }, modifiable));
    }
}

impl Decoder {
    /// A session for `codec` frames of `width`×`height`, described by the
    /// sample-description `atom` (`vpcC`, `av1C`, `avcC`, `hvcC`), made
    /// into IOSurface pixel buffers of `pixel_format`.
    fn new(
        codec: CMVideoCodecType,
        (width, height): (u32, u32),
        atom: (&str, &[u8]),
        tags: Option<&Tags>,
        full_range: Option<bool>,
        pixel_format: u32,
    ) -> std::result::Result<Self, i32> {
        let atoms = CFDictionary::<CFString, CFData>::from_slices(
            &[&*CFString::from_str(atom.0)],
            &[&*CFData::from_bytes(atom.1)],
        );
        let mut keys: Vec<&CFString> =
            vec![unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms }];
        let mut values: Vec<&CFType> = vec![&atoms];
        if let Some(tags) = tags {
            unsafe {
                keys.extend([
                    kCVImageBufferYCbCrMatrixKey,
                    kCVImageBufferColorPrimariesKey,
                    kCVImageBufferTransferFunctionKey,
                ]);
            }
            values.extend([
                tags.matrix as &CFType,
                tags.primaries as &CFType,
                tags.transfer as &CFType,
            ]);
        }
        if let Some(full) = full_range {
            keys.push(unsafe { kCMFormatDescriptionExtension_FullRangeVideo });
            values.push(CFBoolean::new(full));
        }
        let extensions = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        let mut format = ptr::null();
        let status = unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                codec,
                width as i32,
                height as i32,
                Some(extensions.as_opaque()),
                NonNull::from(&mut format),
            )
        };
        let format = NonNull::new(format as *mut CMFormatDescription)
            .filter(|_| status == 0)
            .ok_or(status)?;
        let format = unsafe { CFRetained::from_raw(format) };

        let pixel_format = CFNumber::new_i32(pixel_format as i32);
        let io_surface = CFDictionary::<CFString, CFType>::empty();
        let attributes = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ],
                &[&pixel_format, &io_surface, CFBoolean::new(true)],
            )
        };
        let decoded = Box::new(Mutex::new(Decoded::default()));
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_decoded),
            decompressionOutputRefCon: &*decoded as *const Mutex<Decoded> as *mut c_void,
        };
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut session),
            )
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or(status)?;
        Ok(Self {
            session: unsafe { CFRetained::from_raw(session) },
            format,
            decoded,
        })
    }

    /// Decode one frame (`data`, at `time` ns) and return the pictures it
    /// produced (none for a frame that isn't shown).
    fn decode(
        &mut self,
        data: &[u8],
        time: i64,
    ) -> std::result::Result<Vec<(CFRetained<CVImageBuffer>, bool)>, i32> {
        let mut block = ptr::null_mut();
        // The block points at `data`, which outlives this synchronous
        // decode; kCFAllocatorNull means CoreMedia never frees it.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                data.as_ptr() as *mut c_void,
                data.len(),
                kCFAllocatorNull,
                ptr::null(),
                0,
                data.len(),
                0,
                NonNull::from(&mut block),
            )
        };
        let block = NonNull::new(block).filter(|_| status == 0).ok_or(status)?;
        let block = unsafe { CFRetained::from_raw(block) };
        let timing = CMSampleTimingInfo {
            duration: unsafe { kCMTimeInvalid },
            presentationTimeStamp: unsafe { CMTime::new(time, 1_000_000_000) },
            decodeTimeStamp: unsafe { kCMTimeInvalid },
        };
        let size = data.len();
        let mut sample = ptr::null_mut();
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self.format),
                1,
                1,
                &timing,
                1,
                &size,
                NonNull::from(&mut sample),
            )
        };
        let sample = NonNull::new(sample).filter(|_| status == 0).ok_or(status)?;
        let sample = unsafe { CFRetained::from_raw(sample) };
        let mut info = VTDecodeInfoFlags(0);
        let status = unsafe {
            self.session
                .decode_frame(&sample, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info)
        };
        unsafe { self.session.wait_for_asynchronous_frames() };
        let mut decoded = self.decoded.lock().unwrap();
        if let Some(error) = decoded.error.take() {
            decoded.frames.clear();
            return Err(error);
        }
        if status != 0 {
            decoded.frames.clear();
            return Err(status);
        }
        Ok(decoded.frames.drain(..).collect())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

/// A video track's decoder.
enum Codec {
    Toolbox(Decoder),
    Vp8(Box<Vp8Video>),
}

/// A file's video, decoded.
pub struct VideoReader {
    demuxer: Option<Demuxer>,
    track: u64,
    codec: String,
    /// VideoToolbox's decoder, or VP8's (libvpx).
    color: Option<Decoder>,
    vp8: Option<Box<Vp8Video>>,
    /// Set from the first VP9 alpha frame when the video has alpha.
    alpha: Option<Decoder>,
    has_alpha: bool,
    /// The colour tags to put on each frame (VP9, which VideoToolbox may
    /// not tag the way the stream says).
    tags: Option<Tags>,
    /// BGRA frames for combining colour and alpha into when VideoToolbox's
    /// own buffers can't be written.
    pool: Option<FramePool>,
    /// Decoded frames waiting to be put in presentation order.
    pending: Vec<(i64, PixelBuffer)>,
    reorder: usize,
    ended: bool,
    shown: String,
}

// SAFETY: used by one thread at a time; its decoders may be, and its
// colour tags are immutable CoreVideo constants.
unsafe impl Send for VideoReader {}

impl VideoReader {
    /// Set up decoding `track`'s video, decoding its first frame from
    /// `demuxer` (which the caller then gives the reader to read on).
    fn new(demuxer: &mut Demuxer, track: &Track, shown: &str, container: &str) -> Result<Self> {
        let name = video_codec_name(&track.codec);
        let unsupported = |why: &str| {
            anyhow!(
                "{shown}: {name} video in {container} isn't supported{why}; {}",
                conversion_hint(track.alpha, &track.codec)
            )
        };
        if track.encoded {
            return Err(unsupported(" (its frames use Matroska compression)"));
        }
        let first = demuxer
            .next_packet(track.number)?
            .with_context(|| format!("{shown} has a video track with no frames"))?;
        let size = (track.width, track.height);
        let colour = track.colour;
        let tags_of = |matrix, primaries, transfer| {
            (colour.matrix.is_some() || colour.primaries.is_some() || colour.transfer.is_some())
                .then(|| tags(matrix, primaries, transfer))
        };
        // 8-bit 4:2:0 in video range, like the AVFoundation path asks for;
        // BGRA when alpha is combined in.
        let output = if track.alpha && track.codec == "V_VP9" {
            kCVPixelFormatType_32BGRA
        } else {
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        };
        let failed = |status: i32| unsupported(&format!(" here (VideoToolbox error {status})"));
        let (color, tags, reorder) = match track.codec.as_str() {
            "V_VP8" => {
                // Untagged VP8 is Rec. 709, like untagged video everywhere.
                let tags = tags(colour.matrix, colour.primaries, colour.transfer);
                let vp8 = Vp8Video::new(track.alpha, colour, shown)?;
                (Codec::Vp8(Box::new(vp8)), Some(tags), 0)
            }
            "V_VP9" => {
                static REGISTER: Once = Once::new();
                REGISTER.call_once(|| unsafe {
                    VTRegisterSupplementalVideoDecoderIfAvailable(kCMVideoCodecType_VP9)
                });
                let header = vp9_header(&first.data).with_context(|| {
                    format!("{shown}: its VP9 video doesn't start with a keyframe")
                })?;
                let (primaries, transfer, matrix) = vp9_colour_codes(&header);
                let tags = tags(
                    pick(colour.matrix, matrix),
                    pick(colour.primaries, primaries),
                    pick(colour.transfer, transfer),
                );
                let full = colour.range.map_or(header.full_range, |range| range == 2);
                let decoder = Decoder::new(
                    kCMVideoCodecType_VP9,
                    size,
                    ("vpcC", &vpcc(&header, &colour)),
                    Some(&tags),
                    Some(full),
                    output,
                )
                .map_err(|status| {
                    let chroma = match (header.subsampling_x, header.subsampling_y) {
                        (true, true) => "4:2:0",
                        (true, false) => "4:2:2",
                        _ => "4:4:4",
                    };
                    unsupported(&format!(
                        " here (profile {}, {}-bit {chroma}; VideoToolbox error {status})",
                        header.profile, header.bit_depth
                    ))
                })?;
                (Codec::Toolbox(decoder), Some(tags), 0)
            }
            "V_AV1" => {
                if !unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1) } {
                    return Err(unsupported(
                        " on this Mac (AV1 needs an M3 or later, whose media engine decodes it)",
                    ));
                }
                if track.private.len() < 4 {
                    return Err(unsupported(" (its track has no AV1 configuration)"));
                }
                let tags = tags_of(colour.matrix, colour.primaries, colour.transfer);
                let decoder = Decoder::new(
                    kCMVideoCodecType_AV1,
                    size,
                    ("av1C", &track.private),
                    tags.as_ref(),
                    colour.range.map(|range| range == 2),
                    output,
                )
                .map_err(failed)?;
                (Codec::Toolbox(decoder), tags, 0)
            }
            "V_MPEG4/ISO/AVC" | "V_MPEGH/ISO/HEVC" => {
                let (codec, atom) = if track.codec == "V_MPEG4/ISO/AVC" {
                    (kCMVideoCodecType_H264, "avcC")
                } else {
                    (kCMVideoCodecType_HEVC, "hvcC")
                };
                if track.private.is_empty() {
                    return Err(unsupported(" (its track has no decoder configuration)"));
                }
                let tags = tags_of(colour.matrix, colour.primaries, colour.transfer);
                let decoder = Decoder::new(
                    codec,
                    size,
                    (atom, &track.private),
                    tags.as_ref(),
                    colour.range.map(|range| range == 2),
                    output,
                )
                .map_err(failed)?;
                (Codec::Toolbox(decoder), tags, REORDER_DEPTH)
            }
            _ => return Err(unsupported("")),
        };
        let has_alpha = track.alpha && track.codec == "V_VP9";
        let (color, vp8) = match color {
            Codec::Toolbox(decoder) => (Some(decoder), None),
            Codec::Vp8(vp8) => (None, Some(vp8)),
        };
        let mut reader = Self {
            demuxer: None,
            track: track.number,
            codec: track.codec.clone(),
            color,
            vp8,
            alpha: None,
            has_alpha,
            tags,
            pool: None,
            pending: Vec::new(),
            reorder,
            ended: false,
            shown: shown.to_string(),
        };
        // The first frame must decode, or the file won't play.
        reader
            .decode(first)
            .map_err(|err| anyhow!("{err:#}; {}", conversion_hint(track.alpha, &track.codec)))?;
        Ok(reader)
    }

    fn next(&mut self) -> Result<Option<(PixelBuffer, Duration)>> {
        loop {
            if self.pending.len() > self.reorder || (self.ended && !self.pending.is_empty()) {
                let earliest = (0..self.pending.len())
                    .min_by_key(|&i| self.pending[i].0)
                    .expect("not empty");
                let (time, frame) = self.pending.remove(earliest);
                return Ok(Some((frame, Duration::from_nanos(time.max(0) as u64))));
            }
            if self.ended {
                return Ok(None);
            }
            let demuxer = self.demuxer.as_mut().expect("opened with its demuxer");
            match demuxer.next_packet(self.track)? {
                Some(packet) => self.decode(packet)?,
                None => self.ended = true,
            }
        }
    }

    /// Decode `packet` (and its alpha) into `pending`.
    fn decode(&mut self, packet: Packet) -> Result<()> {
        if let Some(vp8) = self.vp8.as_mut() {
            if let Some(frame) = vp8.decode(&packet.data, packet.alpha.as_deref())? {
                self.push(packet.time, frame);
            }
            return Ok(());
        }
        let name = video_codec_name(&self.codec);
        let frames = self
            .color
            .as_mut()
            .expect("VideoToolbox decodes all but VP8")
            .decode(&packet.data, packet.time)
            .map_err(|status| {
                anyhow!(
                    "could not decode {}'s {name} video (VideoToolbox error {status})",
                    self.shown
                )
            })?;
        let alpha = match (self.has_alpha, &packet.alpha) {
            (true, Some(data)) => self.decode_alpha(data, packet.time)?,
            _ => None,
        };
        for (image, modifiable) in frames {
            let frame = if self.has_alpha {
                self.with_alpha(image, modifiable, alpha.as_ref())?
            } else {
                image
            };
            self.push(packet.time, frame);
        }
        Ok(())
    }

    /// Tag a decoded frame with its colour and queue it.
    fn push(&mut self, time: i64, frame: CFRetained<CVImageBuffer>) {
        if let Some(tags) = &self.tags {
            let mode = CVAttachmentMode::ShouldPropagate;
            unsafe {
                frame.set_attachment(kCVImageBufferYCbCrMatrixKey, tags.matrix, mode);
                frame.set_attachment(kCVImageBufferColorPrimariesKey, tags.primaries, mode);
                frame.set_attachment(kCVImageBufferTransferFunctionKey, tags.transfer, mode);
            }
        }
        self.pending.push((time, PixelBuffer(frame)));
    }

    /// Decode a frame of the VP9 alpha stream, its luma being the alpha.
    fn decode_alpha(
        &mut self,
        data: &[u8],
        time: i64,
    ) -> Result<Option<CFRetained<CVImageBuffer>>> {
        if self.alpha.is_none() {
            // The alpha stream starts at a keyframe of its own, whose
            // header gives its format.
            let Some(header) = vp9_header(data) else {
                return Ok(None);
            };
            // Its native 4:2:0, so VideoToolbox passes the luma through
            // unconverted.
            let format = match (header.bit_depth > 8, header.full_range) {
                (false, false) => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                (false, true) => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                (true, false) => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
                (true, true) => kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
            };
            let decoder = Decoder::new(
                kCMVideoCodecType_VP9,
                self.size(),
                ("vpcC", &vpcc(&header, &Colour::default())),
                None,
                Some(header.full_range),
                format,
            )
            .map_err(|status| {
                anyhow!(
                    "could not decode {}'s VP9 alpha channel (VideoToolbox error {status})",
                    self.shown
                )
            })?;
            self.alpha = Some(decoder);
        }
        let decoder = self.alpha.as_mut().expect("set above");
        let frames = decoder.decode(data, time).map_err(|status| {
            anyhow!(
                "could not decode {}'s VP9 alpha channel (VideoToolbox error {status})",
                self.shown
            )
        })?;
        Ok(frames.into_iter().last().map(|(image, _)| image))
    }

    fn size(&self) -> (u32, u32) {
        let format = &self
            .color
            .as_ref()
            .expect("VP9 has a VideoToolbox decoder")
            .format;
        let dimensions = unsafe { objc2_core_media::CMVideoFormatDescriptionGetDimensions(format) };
        (dimensions.width as u32, dimensions.height as u32)
    }

    /// `color` (BGRA) with `alpha`'s luma as its alpha channel (opaque if
    /// there's none), in `color` itself if VideoToolbox lets it be
    /// written, else in a frame of our own.
    fn with_alpha(
        &mut self,
        color: CFRetained<CVImageBuffer>,
        modifiable: bool,
        alpha: Option<&CFRetained<CVImageBuffer>>,
    ) -> Result<CFRetained<CVImageBuffer>> {
        let target = if modifiable {
            color.clone()
        } else {
            let (width, height) = (
                CVPixelBufferGetWidth(&color) as u32,
                CVPixelBufferGetHeight(&color) as u32,
            );
            if self.pool.is_none() {
                self.pool = Some(FramePool::new(width, height)?);
            }
            let pool = self.pool.as_mut().expect("set above");
            // The pool caps frames in flight; wait briefly for one back.
            let PixelBuffer(target) =
                take_waiting(pool).context("no free frame to combine alpha into")?;
            copy_bgra(&color, &target)?;
            target
        };
        let (width, height) = (
            CVPixelBufferGetWidth(&target),
            CVPixelBufferGetHeight(&target),
        );
        let locked = Locked::new(&target, CVPixelBufferLockFlags(0))?;
        let stride = CVPixelBufferGetBytesPerRow(&target);
        let base = CVPixelBufferGetBaseAddress(&target) as *mut u8;
        if !is_bgra(&target) || base.is_null() || stride < width * 4 {
            bail!(
                "VideoToolbox gave {}'s video in an unexpected format",
                self.shown
            );
        }
        let alpha = alpha.filter(|alpha| alpha_bits(alpha).is_some());
        match alpha {
            Some(alpha) => {
                let _alpha_locked = Locked::new(alpha, CVPixelBufferLockFlags::ReadOnly)?;
                let plane = CVPixelBufferGetBaseAddressOfPlane(alpha, 0) as *const u8;
                let plane_stride = CVPixelBufferGetBytesPerRowOfPlane(alpha, 0);
                let w = width.min(CVPixelBufferGetWidthOfPlane(alpha, 0));
                let h = height.min(CVPixelBufferGetHeightOfPlane(alpha, 0));
                // 10-bit planes hold each sample in a u16's top bits.
                let deep = alpha_bits(alpha) == Some(10);
                let row_bytes = if deep { w * 2 } else { w };
                if plane.is_null() || plane_stride < row_bytes {
                    bail!(
                        "VideoToolbox gave {}'s alpha in an unexpected format",
                        self.shown
                    );
                }
                for y in 0..h {
                    // SAFETY: both buffers are locked, and their rows were
                    // checked to hold this many bytes.
                    let row =
                        unsafe { std::slice::from_raw_parts_mut(base.add(y * stride), w * 4) };
                    let source = unsafe {
                        std::slice::from_raw_parts(plane.add(y * plane_stride), row_bytes)
                    };
                    if deep {
                        let samples = source.as_chunks::<2>().0;
                        for (pixel, sample) in row.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
                            pixel[3] = sample[1];
                        }
                    } else {
                        for (pixel, &a) in row.as_chunks_mut::<4>().0.iter_mut().zip(source) {
                            pixel[3] = a;
                        }
                    }
                }
            }
            None => {
                for y in 0..height {
                    // SAFETY: locked, and each row checked to hold this much.
                    let row =
                        unsafe { std::slice::from_raw_parts_mut(base.add(y * stride), width * 4) };
                    for pixel in row.as_chunks_mut::<4>().0 {
                        pixel[3] = 255;
                    }
                }
            }
        }
        drop(locked);
        Ok(target)
    }
}

/// A pixel buffer's base address locked until dropped.
struct Locked<'a> {
    buffer: &'a CVImageBuffer,
    flags: CVPixelBufferLockFlags,
}

impl<'a> Locked<'a> {
    fn new(buffer: &'a CVImageBuffer, flags: CVPixelBufferLockFlags) -> Result<Self> {
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        if status != 0 {
            bail!("could not read a decoded frame (CoreVideo error {status})");
        }
        Ok(Self { buffer, flags })
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

fn is_bgra(buffer: &CVImageBuffer) -> bool {
    objc2_core_video::CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_32BGRA
}

/// The bits per sample of an alpha frame in one of the 4:2:0 formats the
/// alpha decoder asks for; `None` for anything else.
fn alpha_bits(buffer: &CVImageBuffer) -> Option<u8> {
    let format = objc2_core_video::CVPixelBufferGetPixelFormatType(buffer);
    if format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        || format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
    {
        Some(8)
    } else if format == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        || format == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
    {
        Some(10)
    } else {
        None
    }
}

/// Copy one BGRA buffer's pixels into another.
fn copy_bgra(from: &CVImageBuffer, to: &CVImageBuffer) -> Result<()> {
    let _from = Locked::new(from, CVPixelBufferLockFlags::ReadOnly)?;
    let _to = Locked::new(to, CVPixelBufferLockFlags(0))?;
    let (source, target) = (
        CVPixelBufferGetBaseAddress(from) as *const u8,
        CVPixelBufferGetBaseAddress(to) as *mut u8,
    );
    let (source_stride, target_stride) = (
        CVPixelBufferGetBytesPerRow(from),
        CVPixelBufferGetBytesPerRow(to),
    );
    let width = CVPixelBufferGetWidth(from).min(CVPixelBufferGetWidth(to)) * 4;
    let height = CVPixelBufferGetHeight(from).min(CVPixelBufferGetHeight(to));
    if !is_bgra(from)
        || !is_bgra(to)
        || source.is_null()
        || target.is_null()
        || source_stride < width
        || target_stride < width
    {
        bail!("could not copy a decoded frame: unexpected format");
    }
    for y in 0..height {
        // SAFETY: both are locked, and each row holds at least `width` bytes.
        unsafe {
            ptr::copy_nonoverlapping(
                source.add(y * source_stride),
                target.add(y * target_stride),
                width,
            )
        };
    }
    Ok(())
}

// Audio.

/// Samples (at 48 kHz) in an Opus packet, from its TOC byte.
fn opus_samples(packet: &[u8]) -> u32 {
    let Some(&toc) = packet.first() else {
        return 960;
    };
    let config = toc >> 3;
    // Tenths of a millisecond per frame.
    let tenths = match config {
        0..=11 => [100, 200, 400, 600][(config % 4) as usize],
        12..=15 => [100, 200][(config % 2) as usize],
        _ => [25, 50, 100, 200][(config % 4) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(packet.get(1).map_or(1, |b| b & 0x3f)),
    };
    frames * tenths * 48 / 10
}

/// The AAC decoder's magic cookie for an AudioSpecificConfig (a Matroska
/// AAC track's CodecPrivate): an MPEG-4 ES descriptor around it, as in an
/// MP4's `esds` box.
fn esds(config: &[u8]) -> Vec<u8> {
    fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        // Four-byte lengths, as encoders write them.
        let len = body.len() as u32;
        out.extend([
            0x80 | ((len >> 21) & 0x7f) as u8,
            0x80 | ((len >> 14) & 0x7f) as u8,
            0x80 | ((len >> 7) & 0x7f) as u8,
            (len & 0x7f) as u8,
        ]);
        out.extend_from_slice(body);
        out
    }
    let specific = descriptor(0x05, config);
    // MPEG-4 audio, an audio stream, no buffer size or bitrates.
    let mut decoder_config = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    decoder_config.extend(specific);
    let mut es = vec![0, 0, 0]; // ES_ID, flags
    es.extend(descriptor(0x04, &decoder_config));
    es.extend(descriptor(0x06, &[0x02]));
    descriptor(0x03, &es)
}

/// A file's audio, decoded by an AudioConverter to stereo floats at 48 kHz.
pub struct AudioReader {
    demuxer: Option<Demuxer>,
    track: u64,
    converter: AudioConverterRef,
    channels: u32,
    /// The packet the converter is reading; it stays put until the next
    /// input callback.
    lent: Option<Packet>,
    /// The first packet, read to set up the decoder.
    first: Option<Packet>,
    description: AudioStreamPacketDescription,
    error: Option<anyhow::Error>,
    ended: bool,
    /// Frames still to drop from the start (Opus's pre-skip).
    skip: u64,
    /// Nanoseconds of the first kept sample, and frames returned since.
    start: Option<i64>,
    offset: i64,
    returned: u64,
    /// Where the file ends (ns): sound after it (an encoder's padding) is
    /// dropped, so a looping file's next play doesn't overlap it.
    end: Option<i64>,
    /// Decoding only the first packet, to measure the decoder.
    probing: bool,
    /// Vorbis, decoded here (macOS can't): the converter then gets float
    /// samples, which it only resamples.
    vorbis: Option<Box<Vorbis>>,
}

/// A Vorbis stream's decoder (lewton).
struct Vorbis {
    ident: lewton::header::IdentHeader,
    setup: lewton::header::SetupHeader,
    window: lewton::audio::PreviousWindowRight,
    /// The time of the first packet, which gives no sound itself (Vorbis
    /// starts overlapping from the second): the first sound plays then.
    start: Option<i64>,
    /// Whether sound has come out yet.
    started: bool,
}

impl Vorbis {
    /// The decoder for a track whose CodecPrivate holds the three Vorbis
    /// headers, Xiph-laced.
    fn new(private: &[u8]) -> std::result::Result<Self, String> {
        let [identification, comment, setup] = xiph_headers(private)
            .ok_or_else(|| "its track has no valid Vorbis headers".to_string())?;
        let ident = lewton::header::read_header_ident(identification)
            .map_err(|err| format!("its Vorbis headers are damaged ({err:?})"))?;
        lewton::header::read_header_comment(comment)
            .map_err(|err| format!("its Vorbis headers are damaged ({err:?})"))?;
        let setup = lewton::header::read_header_setup(
            setup,
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        )
        .map_err(|err| format!("its Vorbis headers are damaged ({err:?})"))?;
        Ok(Self {
            ident,
            setup,
            window: lewton::audio::PreviousWindowRight::new(),
            start: None,
            started: false,
        })
    }

    /// One packet's samples, interleaved.
    fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>> {
        let Self {
            ident,
            setup,
            window,
            ..
        } = self;
        // A damaged packet must not take the engine down with it.
        let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lewton::audio::read_audio_packet_generic::<lewton::samples::InterleavedSamples<f32>>(
                ident, setup, packet, window,
            )
        }))
        .map_err(|_| anyhow!("the Vorbis decoder failed on a damaged packet"))?
        .map_err(|err| anyhow!("could not decode the Vorbis audio ({err:?})"))?;
        // A damaged packet can decode far out of range; keep it to full scale.
        let mut samples = decoded.samples;
        for sample in &mut samples {
            *sample = sample.clamp(-1.0, 1.0);
        }
        Ok(samples)
    }
}

/// The three packets of a Xiph-laced CodecPrivate (Vorbis's headers).
fn xiph_headers(private: &[u8]) -> Option<[&[u8]; 3]> {
    let (&count, mut rest) = private.split_first()?;
    if count != 2 {
        return None;
    }
    let mut sizes = [0usize; 2];
    for size in &mut sizes {
        loop {
            let (&byte, tail) = rest.split_first()?;
            rest = tail;
            *size += usize::from(byte);
            if byte < 255 {
                break;
            }
        }
    }
    let (first, rest) = rest.split_at_checked(sizes[0])?;
    let (second, third) = rest.split_at_checked(sizes[1])?;
    Some([first, second, third])
}

// SAFETY: an AudioConverter may be used from any thread, one at a time.
unsafe impl Send for AudioReader {}

impl Drop for AudioReader {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

impl AudioReader {
    fn new(demuxer: &mut Demuxer, track: &Track, shown: &str, container: &str) -> Result<Self> {
        let name = audio_codec_name(&track.codec);
        let unsupported = |why: &str| {
            anyhow!(
                "{shown}: {name} audio in {container} isn't supported{why} (convert the sound: \
                 ffmpeg -i IN -c:v copy -c:a libopus OUT.webm)"
            )
        };
        if track.encoded {
            return Err(unsupported(" (its frames use Matroska compression)"));
        }
        if !(1..=2).contains(&track.channels) {
            return Err(unsupported(&format!(" with {} channels", track.channels)));
        }
        let first = demuxer
            .next_packet(track.number)?
            .with_context(|| format!("{shown} has an audio track with no sound"))?;
        let channels = track.channels;
        let mut vorbis = None;
        let (input, cookie, skip, offset) = match track.codec.as_str() {
            "A_OPUS" => {
                // OpusHead: the pre-skip is a little-endian u16 at byte 10.
                let pre_skip = match track.private.get(10..12) {
                    Some(bytes) => u64::from(u16::from_le_bytes([bytes[0], bytes[1]])),
                    None => track.codec_delay * 48_000 / 1_000_000_000,
                };
                let input = AudioStreamBasicDescription {
                    mSampleRate: 48_000.0,
                    mFormatID: kAudioFormatOpus,
                    mFormatFlags: 0,
                    mBytesPerPacket: 0,
                    mFramesPerPacket: opus_samples(&first.data),
                    mBytesPerFrame: 0,
                    mChannelsPerFrame: channels,
                    mBitsPerChannel: 0,
                    mReserved: 0,
                };
                // Samples before the pre-skip are dropped; the first kept
                // one plays at the first block's time less the codec delay
                // plus the pre-skip (Matroska's Opus mapping).
                let offset = (pre_skip * 1_000_000_000 / 48_000) as i64 - track.codec_delay as i64;
                (input, None, pre_skip, offset)
            }
            codec if codec.starts_with("A_AAC") => {
                if track.private.is_empty() {
                    return Err(unsupported(" (its track has no AAC configuration)"));
                }
                let input = AudioStreamBasicDescription {
                    mSampleRate: track.sample_rate,
                    mFormatID: kAudioFormatMPEG4AAC,
                    mFormatFlags: 0,
                    mBytesPerPacket: 0,
                    mFramesPerPacket: 1024,
                    mBytesPerFrame: 0,
                    mChannelsPerFrame: channels,
                    mBitsPerChannel: 0,
                    mReserved: 0,
                };
                // The encoder's priming samples (CodecDelay), dropped so
                // the first kept sample plays at the first block's time.
                let priming = track.codec_delay * u64::from(SAMPLE_RATE) / 1_000_000_000;
                (input, Some(esds(&track.private)), priming, 0)
            }
            "A_VORBIS" => {
                let decoder =
                    Vorbis::new(&track.private).map_err(|why| unsupported(&format!(" ({why})")))?;
                if u32::from(decoder.ident.audio_channels) != channels {
                    return Err(unsupported(" (its track and stream disagree on channels)"));
                }
                // Decoded to floats here; the converter only resamples.
                let input = AudioStreamBasicDescription {
                    mSampleRate: f64::from(decoder.ident.audio_sample_rate),
                    mFormatID: kAudioFormatLinearPCM,
                    mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                    mBytesPerPacket: 4 * channels,
                    mFramesPerPacket: 1,
                    mBytesPerFrame: 4 * channels,
                    mChannelsPerFrame: channels,
                    mBitsPerChannel: 32,
                    mReserved: 0,
                };
                vorbis = Some(Box::new(decoder));
                (input, None, 0, 0)
            }
            _ => return Err(unsupported("")),
        };
        let output = AudioStreamBasicDescription {
            mSampleRate: f64::from(SAMPLE_RATE),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 4 * channels,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * channels,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let new_converter = || {
            let mut converter: AudioConverterRef = ptr::null_mut();
            let status = unsafe {
                AudioConverterNew(
                    NonNull::from(&input),
                    NonNull::from(&output),
                    NonNull::from(&mut converter),
                )
            };
            if status != 0 || converter.is_null() {
                return Err(unsupported(&format!(" here (AudioToolbox error {status})")));
            }
            Ok(converter)
        };
        let converter = new_converter()?;
        let mut reader = Self {
            demuxer: None,
            track: track.number,
            converter,
            channels,
            lent: None,
            first: Some(first),
            description: AudioStreamPacketDescription {
                mStartOffset: 0,
                mVariableFramesInPacket: 0,
                mDataByteSize: 0,
            },
            error: None,
            ended: false,
            skip,
            start: None,
            offset,
            returned: 0,
            end: None,
            probing: false,
            vorbis,
        };
        if let Some(cookie) = cookie {
            let status = unsafe {
                AudioConverterSetProperty(
                    converter,
                    kAudioConverterDecompressionMagicCookie,
                    cookie.len() as u32,
                    NonNull::new(cookie.as_ptr() as *mut c_void).expect("not empty"),
                )
            };
            if status != 0 {
                return Err(unsupported(&format!(
                    " (AudioToolbox rejected its configuration, error {status})"
                )));
            }
        }
        if track.codec == "A_OPUS" {
            // Apple's Opus decoder drops the start of what it decodes
            // itself (120 samples on macOS 27), which counts toward the
            // pre-skip. Decode the first packet alone to see how much.
            let expected = opus_samples(&reader.first.as_ref().expect("read above").data);
            reader.probing = true;
            let decoded = reader.fill(4 * 5760);
            reader.probing = false;
            // A fresh decoder for playing: a reset one doesn't drop the
            // start again.
            let fresh = new_converter()?;
            unsafe { AudioConverterDispose(reader.converter) };
            reader.converter = fresh;
            reader.first = reader.lent.take();
            reader.start = None;
            let decoded =
                decoded.with_context(|| format!("{shown}: could not decode its Opus audio"))?;
            let trimmed = expected.saturating_sub((decoded.len() / channels as usize) as u32);
            reader.skip = reader.skip.saturating_sub(u64::from(trimmed));
        }
        Ok(reader)
    }

    /// Decode up to `capacity` frames, fewer at the end of the stream.
    fn fill(&mut self, capacity: u32) -> Result<Vec<f32>> {
        let channels = self.channels as usize;
        let mut samples = vec![0f32; capacity as usize * channels];
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: self.channels,
                mDataByteSize: (samples.len() * size_of::<f32>()) as u32,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        let mut frames = capacity;
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                Some(supply),
                (self as *mut Self).cast(),
                NonNull::from(&mut frames),
                NonNull::from(&mut list),
                ptr::null_mut(),
            )
        };
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        if status != 0 {
            bail!("could not decode the audio (AudioToolbox error {status})");
        }
        samples.truncate(frames as usize * channels);
        Ok(samples)
    }

    /// The next packet for the converter, `None` at the end: the file's,
    /// or for Vorbis, its decoded samples (floats).
    fn packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let packet = match self.first.take() {
                Some(first) => Some(first),
                None => {
                    let demuxer = self.demuxer.as_mut().expect("opened with its demuxer");
                    demuxer.next_packet(self.track)?
                }
            };
            let Some(mut packet) = packet else {
                return Ok(None);
            };
            let Some(vorbis) = self.vorbis.as_mut() else {
                return Ok(Some(packet));
            };
            let samples = vorbis.decode(&packet.data)?;
            if samples.is_empty() {
                if !vorbis.started {
                    vorbis.start.get_or_insert(packet.time);
                }
                continue;
            }
            if !vorbis.started {
                vorbis.started = true;
                packet.time = vorbis.start.unwrap_or(packet.time);
            }
            packet.data = samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect();
            return Ok(Some(packet));
        }
    }

    fn next(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        let channels = self.channels as usize;
        loop {
            if self.ended {
                return Ok(None);
            }
            let mut samples = self.fill(AUDIO_CHUNK)?;
            let frames = (samples.len() / channels) as u64;
            if frames == 0 {
                self.ended = true;
                return Ok(None);
            }
            let skip = self.skip.min(frames);
            self.skip -= skip;
            samples.drain(..skip as usize * channels);
            if samples.is_empty() {
                continue;
            }
            let start = self.start.unwrap_or(0);
            let at =
                start + (self.returned as i128 * 1_000_000_000 / i128::from(SAMPLE_RATE)) as i64;
            if let Some(end) = self.end {
                let left =
                    ((end - at).max(0) as i128 * i128::from(SAMPLE_RATE) / 1_000_000_000) as usize;
                if left == 0 {
                    self.ended = true;
                    return Ok(None);
                }
                samples.truncate(left.min(samples.len() / channels) * channels);
            }
            self.returned += (samples.len() / channels) as u64;
            let stereo = if channels == 1 {
                samples.iter().flat_map(|&s| [s, s]).collect()
            } else {
                samples
            };
            return Ok(Some((stereo, Duration::from_nanos(at.max(0) as u64))));
        }
    }
}

/// The converter's input callback: the next packet of the track.
unsafe extern "C-unwind" fn supply(
    _converter: AudioConverterRef,
    count: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    // SAFETY: `user` is the reader filling the buffer.
    let reader = unsafe { &mut *(user as *mut AudioReader) };
    let list = unsafe { &mut *data.as_ptr() };
    // Probing, the first packet is all there is.
    let packet = match reader.probing && reader.lent.is_some() {
        true => Ok(None),
        false => reader.packet(),
    };
    let packet = match packet {
        Ok(packet) => packet,
        Err(err) => {
            reader.error = Some(err);
            None
        }
    };
    let Some(packet) = packet else {
        // No packets and no error: the end of the stream, which lets the
        // converter flush what it holds.
        unsafe { *count.as_ptr() = 0 };
        list.mBuffers[0].mDataByteSize = 0;
        list.mBuffers[0].mData = ptr::null_mut();
        return if reader.error.is_some() {
            READ_FAILED
        } else {
            0
        };
    };
    if reader.start.is_none() {
        reader.start = Some(packet.time + reader.offset);
    }
    reader.description.mDataByteSize = packet.data.len() as u32;
    list.mBuffers[0].mNumberChannels = reader.channels;
    list.mBuffers[0].mDataByteSize = packet.data.len() as u32;
    list.mBuffers[0].mData = packet.data.as_ptr() as *mut c_void;
    // Decoded Vorbis is linear PCM, whose packets are its frames.
    let packets = match reader.vorbis {
        Some(_) => packet.data.len() as u32 / (4 * reader.channels),
        None => 1,
    };
    if !descriptions.is_null() && reader.vorbis.is_none() {
        unsafe { *descriptions = &mut reader.description };
    }
    unsafe { *count.as_ptr() = packets };
    // Kept until the next call, while the converter reads it.
    reader.lent = Some(packet);
    0
}
