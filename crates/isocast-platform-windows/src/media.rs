//! Decoding video and audio files with Media Foundation's Source Reader.
//!
//! Pictures are decoded on the engine's D3D11 device where it can
//! (hardware decoders and the GPU video processor, through a DXGI device
//! manager) into BGRA textures, copied on the GPU into the reader's own
//! bounded pool. Where that path can't open a file (a software adapter
//! without video processing, say), Media Foundation decodes and converts on
//! the CPU and the picture is uploaded once. Sound comes out as interleaved
//! stereo float at the engine's sample rate through the reader's own
//! resampler. Looping and restarts stay in the engine: it opens a reader
//! for each play.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{MediaInfo, MediaPart, SAMPLE_RATE};
use isocast_protocol::mp4_edits::{self, Edit, Edits};
use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, HSTRING, Interface};

use crate::frames::TexturePool;
use crate::mf::Runtime;
use crate::{Frame, GraphicsDevice};

/// Sound within this of continuing the previous chunk continues it
/// exactly: Media Foundation's 100 ns times (and its resampler) round, and
/// a gap of a few samples would be heard. A real jump keeps its own time,
/// so an offset never exceeds this and is never carried forward.
const ALIGNMENT: Duration = Duration::from_millis(5);

/// The same, in Media Foundation's 100 ns units.
const ALIGNMENT_HNS: i64 = 50_000;

fn hns(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX)
}

/// The `MediaPart`'s first stream, as the Source Reader names it.
fn stream(part: MediaPart) -> u32 {
    match part {
        MediaPart::Video => MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
        MediaPart::Audio => MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
    }
}

fn noun(part: MediaPart) -> &'static str {
    match part {
        MediaPart::Video => "video",
        MediaPart::Audio => "audio",
    }
}

/// A file being decoded, one part of it, from its start.
pub struct Reader {
    reader: IMFSourceReader,
    part: MediaPart,
    path: String,
    file: PathBuf,
    video: Option<Video>,
    /// Where the last chunk of sound ended.
    audio_end: Option<Duration>,
    /// The next chunk of sound, read ahead.
    ahead: Option<(Vec<f32>, Duration)>,
    /// A QuickTime/MP4 file's edit for this part, and what is taken from
    /// Media Foundation's times to apply it (found at the first sample).
    edit: Option<Edit>,
    rebase: Option<i64>,
    /// The file's length: the engine plays the next file (or loop) from
    /// there, so nothing is given past it (Windows 10 gives an MP4's last
    /// AAC frame whole, past the edit's end).
    end: Option<Duration>,
    ended: bool,
}

// SAFETY: the Source Reader is free-threaded and lives in the process MTA,
// which `native::process_mta` keeps for the life of the process; threads
// that never initialize COM use it as implicit MTA members. The engine uses
// a reader from one thread at a time. Its textures belong to the engine's
// multithread-protected device, and pool slots are guarded by their own
// reference counts as for every other Windows frame.
unsafe impl Send for Reader {}

struct Video {
    device: GraphicsDevice,
    pool: TexturePool,
    /// Decoded on the GPU (textures) or the CPU (system memory).
    gpu: bool,
    /// The picture as shown: the file's display size within the frame.
    size: (u32, u32),
    /// The rows' stride of a CPU picture in a plain buffer (negative:
    /// bottom-up).
    stride: i32,
    /// GPU samples whose texture a pool copy may still read.
    retained: VecDeque<(IMFSample, Frame)>,
    /// CPU pictures, with opaque alpha, before their upload.
    pixels: Vec<u8>,
    /// The first picture, read while opening.
    first: Option<(Frame, Duration)>,
}

/// Files whose GPU decoding failed in this process: they go straight to
/// the CPU path on later plays (each loop opens a reader).
static CPU_ONLY: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

fn decode_on_cpu(path: &Path) {
    let mut paths = CPU_ONLY.lock();
    let paths = paths.get_or_insert_with(HashSet::new);
    if paths.len() >= 256 {
        paths.clear();
    }
    paths.insert(path.to_path_buf());
}

/// A file's edits, by path, size and modification time: each loop opens
/// readers again, and a long file's movie header is megabytes.
type EditCache = HashMap<PathBuf, (u64, Option<std::time::SystemTime>, Option<Edits>)>;
static EDITS: Mutex<Option<EditCache>> = Mutex::new(None);

fn edits(path: &Path) -> Option<Edits> {
    let metadata = std::fs::metadata(path).ok()?;
    let stamp = (metadata.len(), metadata.modified().ok());
    if let Some((len, modified, edits)) = EDITS.lock().as_ref().and_then(|cache| cache.get(path))
        && (*len, *modified) == stamp
    {
        return *edits;
    }
    let edits = mp4_edits::read(path);
    let mut cache = EDITS.lock();
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (stamp.0, stamp.1, edits));
    edits
}

/// A time in Media Foundation's 100 ns units as a duration from the file's
/// start (negative counts as the start), refusing times no file has.
fn from_hns(time: i64, shown: &str) -> Result<Duration> {
    let nanos = u64::try_from(time.max(0))
        .ok()
        .and_then(|time| time.checked_mul(100))
        .with_context(|| format!("could not read {shown}: sample time {time} is out of range"))?;
    Ok(Duration::from_nanos(nanos))
}

/// How long `frames` of sound at the engine's rate last.
fn frames_duration(frames: usize) -> Duration {
    Duration::from_nanos(
        u64::try_from(frames as u128 * 1_000_000_000 / u128::from(SAMPLE_RATE)).unwrap_or(u64::MAX),
    )
}

/// `at + length`, refusing an overflow.
fn after(at: Duration, length: Duration, shown: &str) -> Result<Duration> {
    at.checked_add(length)
        .with_context(|| format!("could not read {shown}: sample time {at:?} is out of range"))
}

fn start() -> Result<()> {
    crate::native::process_mta()?;
    Runtime::new()?;
    Ok(())
}

fn attributes(count: u32) -> Result<IMFAttributes> {
    let mut attributes = None;
    unsafe { MFCreateAttributes(&mut attributes, count) }?;
    attributes.context("MF returned no attributes")
}

/// A Windows error with its text and code.
fn reason(error: &windows::core::Error) -> String {
    let text = error.message();
    let text = text.trim();
    if text.is_empty() {
        format!("{:#010x}", error.code().0 as u32)
    } else {
        format!("{text} ({:#010x})", error.code().0 as u32)
    }
}

fn source_reader(path: &Path, shown: &str, attributes: &IMFAttributes) -> Result<IMFSourceReader> {
    unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), attributes) }.map_err(
        |error| {
            if error.code() == MF_E_UNSUPPORTED_BYTESTREAM_TYPE
                || error.code() == MF_E_INVALID_FILE_FORMAT
            {
                let web = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        ["webm", "ogg", "ogv", "oga", "opus"]
                            .iter()
                            .any(|web| ext.eq_ignore_ascii_case(web))
                    });
                let fix = if web {
                    "install \"Web Media Extensions\" from the Microsoft Store (ms-windows-store://pdp/?ProductId=9N5TDP8VCMHS), or use an MP4 file"
                } else {
                    "use an MP4, MOV, MKV, WAV or MP3 file"
                };
                anyhow!(
                    "could not read {shown}: Windows can't open this kind of file ({}); {fix}",
                    reason(&error)
                )
            } else {
                anyhow!("could not read {shown}: {}", reason(&error))
            }
        },
    )
}

/// The native (file) type of `part`'s first stream, or "has no …".
fn native_type(reader: &IMFSourceReader, part: MediaPart, shown: &str) -> Result<IMFMediaType> {
    match unsafe { reader.GetNativeMediaType(stream(part), 0) } {
        Ok(native) => Ok(native),
        Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => {
            bail!("{shown} has no {}", noun(part))
        }
        Err(error) => bail!(
            "could not read {shown}'s {}: {}",
            noun(part),
            reason(&error)
        ),
    }
}

/// Ask the reader for `part` as the engine takes it: BGRA pictures
/// (`gpu`: ARGB32 textures, else RGB32 in memory) or 48 kHz stereo float.
/// Returns the type it gives; a missing decoder names the file's codec.
fn set_output(
    reader: &IMFSourceReader,
    part: MediaPart,
    shown: &str,
    gpu: bool,
) -> Result<(IMFMediaType, IMFMediaType)> {
    let native = native_type(reader, part, shown)?;
    let wanted = unsafe { MFCreateMediaType() }?;
    unsafe {
        match part {
            MediaPart::Video => {
                wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                wanted.SetGUID(
                    &MF_MT_SUBTYPE,
                    if gpu {
                        &MFVideoFormat_ARGB32
                    } else {
                        &MFVideoFormat_RGB32
                    },
                )?;
            }
            MediaPart::Audio => {
                wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
                wanted.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
                wanted.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                wanted.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)?;
                wanted.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 8)?;
                wanted.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * 8)?;
                // Front left and right.
                wanted.SetUINT32(&MF_MT_AUDIO_CHANNEL_MASK, 0x3)?;
            }
        }
    }
    let index = stream(part);
    if let Err(error) = unsafe { reader.SetCurrentMediaType(index, None, &wanted) } {
        return Err(no_decoder(shown, part, &native, &error));
    }
    if part == MediaPart::Video {
        keep_frame_rate(reader);
    }
    let current = unsafe { reader.GetCurrentMediaType(index) }?;
    if part == MediaPart::Audio {
        check_audio(&current, shown)?;
    }
    Ok((native, current))
}

/// The sound is what the engine takes: 48 kHz stereo float, also after a
/// change of type mid-file.
fn check_audio(current: &IMFMediaType, shown: &str) -> Result<()> {
    let rate = unsafe { current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }?;
    let channels = unsafe { current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }?;
    let subtype = unsafe { current.GetGUID(&MF_MT_SUBTYPE) }?;
    if (rate, channels, subtype) != (SAMPLE_RATE, 2, MFAudioFormat_Float) {
        bail!(
            "could not read {shown}'s audio: Windows gave {rate} Hz, {channels} channels instead of {SAMPLE_RATE} Hz stereo float"
        );
    }
    Ok(())
}

/// Every decoded picture, at its own time: the video processor would
/// otherwise convert to the frame rate the source guessed (a WebM file
/// says 12.5 fps on Windows 10, and its 25 fps pictures came out halved).
fn keep_frame_rate(reader: &IMFSourceReader) {
    let Ok(reader) = reader.cast::<IMFSourceReaderEx>() else {
        return;
    };
    for index in 0..8 {
        let mut category = GUID::zeroed();
        let mut transform = None;
        let found = unsafe {
            reader.GetTransformForStream(
                stream(MediaPart::Video),
                index,
                Some(&mut category),
                &mut transform,
            )
        };
        if found.is_err() {
            break;
        }
        if category == MFT_CATEGORY_VIDEO_PROCESSOR
            && let Some(attributes) = transform.and_then(|t| unsafe { t.GetAttributes() }.ok())
        {
            let _ = unsafe { attributes.SetUINT32(&MF_XVP_DISABLE_FRC, 1) };
        }
    }
}

/// A codec's name, and how to get Windows a decoder for it.
struct Codec {
    name: String,
    fix: &'static str,
}

const MEDIA_FEATURE_PACK: &str = "it is part of Windows; on Windows N editions install the Media Feature Pack (Settings › Apps › Optional features)";
const WEB_MEDIA: &str = "install \"Web Media Extensions\" from the Microsoft Store (ms-windows-store://pdp/?ProductId=9N5TDP8VCMHS)";
const CONVERT: &str = "convert the file to H.264 video with AAC sound in an MP4";

fn codec(subtype: GUID) -> Codec {
    let known: &[(GUID, &str, &'static str)] = &[
        (MFVideoFormat_H264, "H.264", MEDIA_FEATURE_PACK),
        (
            MFVideoFormat_HEVC,
            "HEVC",
            "install \"HEVC Video Extensions\" from the Microsoft Store (ms-windows-store://pdp/?ProductId=9NMZLZ57R3T7)",
        ),
        (
            MFVideoFormat_VP90,
            "VP9",
            "install \"VP9 Video Extensions\" from the Microsoft Store (ms-windows-store://pdp/?ProductId=9N4D0MSMP0PT)",
        ),
        (MFVideoFormat_VP80, "VP8", WEB_MEDIA),
        (
            MFVideoFormat_AV1,
            "AV1",
            "install \"AV1 Video Extension\" from the Microsoft Store (ms-windows-store://pdp/?ProductId=9MVZQVXJBQ9V)",
        ),
        (
            MFVideoFormat_MPEG2,
            "MPEG-2",
            "install \"MPEG-2 Video Extension\" from the Microsoft Store (ms-windows-store://pdp/?ProductId=9N95Q1ZZPMH4)",
        ),
        (MFVideoFormat_MP4V, "MPEG-4 Part 2", MEDIA_FEATURE_PACK),
        (MFVideoFormat_MJPG, "Motion JPEG", MEDIA_FEATURE_PACK),
        (MFVideoFormat_WMV3, "WMV 9", MEDIA_FEATURE_PACK),
        (MFAudioFormat_AAC, "AAC", MEDIA_FEATURE_PACK),
        (MFAudioFormat_MP3, "MP3", MEDIA_FEATURE_PACK),
        (MFAudioFormat_FLAC, "FLAC", MEDIA_FEATURE_PACK),
        (MFAudioFormat_ALAC, "ALAC", MEDIA_FEATURE_PACK),
        (
            MFAudioFormat_Dolby_AC3,
            "Dolby Digital (AC-3)",
            MEDIA_FEATURE_PACK,
        ),
        (MFAudioFormat_Opus, "Opus", WEB_MEDIA),
        (MFAudioFormat_Vorbis, "Vorbis", WEB_MEDIA),
        (MFAudioFormat_PCM, "PCM", MEDIA_FEATURE_PACK),
        (MFAudioFormat_Float, "float PCM", MEDIA_FEATURE_PACK),
    ];
    if let Some(&(_, name, fix)) = known.iter().find(|(guid, _, _)| *guid == subtype) {
        return Codec {
            name: name.into(),
            fix,
        };
    }
    // Most Media Foundation subtypes are a FOURCC in the base GUID.
    let fourcc = subtype.data1.to_le_bytes();
    let base = GUID::from_u128(0x00000000_0000_0010_8000_00aa00389b71);
    let name = if (subtype.data2, subtype.data3, subtype.data4)
        == (base.data2, base.data3, base.data4)
        && fourcc.iter().all(|c| c.is_ascii_graphic() || *c == b' ')
    {
        String::from_utf8_lossy(&fourcc).trim().to_string()
    } else {
        format!("{subtype:?}")
    };
    Codec { name, fix: CONVERT }
}

/// Whether any decoder transform for `subtype` is installed, Store codec
/// extensions included.
pub(crate) fn decoder_installed(part: MediaPart, subtype: GUID) -> bool {
    let (category, major) = match part {
        MediaPart::Video => (MFT_CATEGORY_VIDEO_DECODER, MFMediaType_Video),
        MediaPart::Audio => (MFT_CATEGORY_AUDIO_DECODER, MFMediaType_Audio),
    };
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: subtype,
    };
    let mut list = std::ptr::null_mut();
    let mut count = 0;
    let found = unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_ALL | MFT_ENUM_FLAG_UNTRUSTED_STOREMFT,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
    };
    if !list.is_null() {
        for item in unsafe { std::slice::from_raw_parts_mut(list, count as usize) } {
            drop(item.take());
        }
        unsafe { CoTaskMemFree(Some(list.cast())) };
    }
    found.is_ok() && count > 0
}

fn no_decoder(
    shown: &str,
    part: MediaPart,
    native: &IMFMediaType,
    error: &windows::core::Error,
) -> anyhow::Error {
    let Ok(subtype) = (unsafe { native.GetGUID(&MF_MT_SUBTYPE) }) else {
        return anyhow!(
            "could not decode {shown}'s {}: {}",
            noun(part),
            reason(error)
        );
    };
    let codec = codec(subtype);
    let uncompressed = subtype == MFAudioFormat_PCM || subtype == MFAudioFormat_Float;
    if !uncompressed && !decoder_installed(part, subtype) {
        anyhow!(
            "{shown}: Windows has no decoder for its {} {}; {}, then reopen the source",
            codec.name,
            noun(part),
            codec.fix
        )
    } else {
        anyhow!(
            "could not decode {shown}'s {} {}: {}",
            codec.name,
            noun(part),
            reason(error)
        )
    }
}

fn size(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    let (width, height) = ((size >> 32) as u32, size as u32);
    (width > 0 && height > 0).then_some((width, height))
}

fn aperture(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let mut area = MFVideoArea::default();
    let mut length = 0;
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut area as *mut MFVideoArea).cast::<u8>(),
            size_of::<MFVideoArea>(),
        )
    };
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, Some(&mut length)) }
        .ok()?;
    let (width, height) = (area.Area.cx, area.Area.cy);
    (length as usize == size_of::<MFVideoArea>()
        && area.OffsetX.value == 0
        && area.OffsetY.value == 0
        && width > 0
        && height > 0)
        .then_some((width as u32, height as u32))
}

/// The decoded frame's size, and the picture's size within it as shown:
/// the display aperture, else the file's own frame size (a decoder's frame
/// can be padded to whole macroblocks, 1088 rows for 1080).
fn layout(
    native: &IMFMediaType,
    current: &IMFMediaType,
    shown: &str,
) -> Result<((u32, u32), (u32, u32))> {
    let frame = size(current).with_context(|| format!("could not read {shown}'s picture size"))?;
    let shown_size = aperture(current)
        .or_else(|| aperture(native))
        .or_else(|| size(native))
        .unwrap_or(frame);
    Ok((
        frame,
        (shown_size.0.min(frame.0), shown_size.1.min(frame.1)),
    ))
}

/// What `path` holds. Its decoders are found now, so a file Windows can't
/// play fails here, naming its codec; any part that can't be decoded fails
/// the whole file rather than leaving a source silently blank or silent.
pub fn media_info(path: &Path) -> Result<MediaInfo> {
    let shown = path.display().to_string();
    start()?;
    if !path.exists() {
        bail!("{shown} does not exist");
    }
    let attributes = attributes(1)?;
    unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }?;
    let reader = source_reader(path, &shown, &attributes)?;
    let has = |part| match unsafe { reader.GetNativeMediaType(stream(part), 0) } {
        Ok(_) => Ok(true),
        Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => Ok(false),
        Err(error) => Err(anyhow!(
            "could not read {shown}'s {}: {}",
            noun(part),
            reason(&error)
        )),
    };
    let video = if has(MediaPart::Video)? {
        let (native, current) = set_output(&reader, MediaPart::Video, &shown, false)?;
        Some(layout(&native, &current, &shown)?.1)
    } else {
        None
    };
    let audio = has(MediaPart::Audio)?;
    if audio {
        set_output(&reader, MediaPart::Audio, &shown, false)?;
    }
    if video.is_none() && !audio {
        bail!("{shown} has no video or audio Windows can play");
    }
    let Some(duration) = duration(&reader) else {
        bail!("{shown}: duration unknown; media sources need a seekable file with a known length");
    };
    Ok(MediaInfo {
        video,
        audio,
        duration,
    })
}

/// How long the file plays, if known.
fn duration(reader: &IMFSourceReader) -> Option<Duration> {
    let value = unsafe {
        reader.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
    }
    .ok()?;
    let ticks = u64::try_from(&value).ok().filter(|ticks| *ticks > 0)?;
    Some(Duration::from_nanos(ticks.saturating_mul(100)))
}

/// Open `part` of `path` to decode it from its start. Pictures need the
/// engine's `device`.
pub fn open(device: Option<GraphicsDevice>, path: &Path, part: MediaPart) -> Result<Reader> {
    let shown = path.display().to_string();
    start()?;
    if !path.exists() {
        bail!("{shown} does not exist");
    }
    match part {
        MediaPart::Audio => {
            let reader = source_reader(path, &shown, &attributes(1)?)?;
            select(&reader, part, &shown)?;
            set_output(&reader, part, &shown, false)?;
            let end = duration(&reader);
            Ok(Reader {
                reader,
                part,
                path: shown,
                file: path.to_path_buf(),
                video: None,
                audio_end: None,
                ahead: None,
                edit: edits(path).map(|edits| edits.audio),
                rebase: None,
                end,
                ended: false,
            })
        }
        MediaPart::Video => {
            let device = device.context("media.frame: no D3D11 device for decoded pictures")?;
            let cpu_only = CPU_ONLY
                .lock()
                .as_ref()
                .is_some_and(|paths| paths.contains(path));
            if !cpu_only {
                match open_video(&device, path, &shown, true) {
                    Ok(reader) => return Ok(reader),
                    Err(gpu) => {
                        // On failure, the CPU's reason: it names a missing
                        // decoder without the GPU path's details.
                        let reader = open_video(&device, path, &shown, false)?;
                        isocast_diagnostics::diagnostic!("info";
                            "media {shown}: GPU decoding unavailable ({gpu:#}); decoding on the CPU"
                        );
                        decode_on_cpu(path);
                        return Ok(reader);
                    }
                }
            }
            open_video(&device, path, &shown, false)
        }
    }
}

/// Decode only `part`'s first stream.
fn select(reader: &IMFSourceReader, part: MediaPart, shown: &str) -> Result<()> {
    native_type(reader, part, shown)?;
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream(part), true)?;
    }
    Ok(())
}

/// A video reader's attributes: Media Foundation's video processor, and on
/// the GPU (`gpu`) the engine's device and hardware decoders.
fn video_attributes(device: &GraphicsDevice, gpu: bool) -> Result<IMFAttributes> {
    let attributes = attributes(3)?;
    unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }?;
    if gpu {
        let mut token = 0;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }?;
        let manager = manager.context("MF returned no DXGI device manager")?;
        unsafe { manager.ResetDevice(device.raw(), token) }?;
        unsafe {
            attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
            attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        }
    }
    Ok(attributes)
}

/// Open the pictures, decoded on the GPU (`gpu`) or the CPU, and read the
/// first: a path that opens but can't decode fails here, not mid-play.
fn open_video(device: &GraphicsDevice, path: &Path, shown: &str, gpu: bool) -> Result<Reader> {
    let reader = source_reader(path, shown, &video_attributes(device, gpu)?)?;
    select(&reader, MediaPart::Video, shown)?;
    let (native, current) = set_output(&reader, MediaPart::Video, shown, gpu)?;
    let (frame, size) = layout(&native, &current, shown)?;
    let stride = default_stride(&current, frame.0)?;
    let end = duration(&reader);
    let mut reader = Reader {
        reader,
        part: MediaPart::Video,
        path: shown.to_string(),
        file: path.to_path_buf(),
        video: Some(Video {
            device: device.clone(),
            pool: TexturePool::new(device.clone()),
            gpu,
            size,
            stride,
            retained: VecDeque::new(),
            pixels: Vec::new(),
            first: None,
        }),
        audio_end: None,
        ahead: None,
        edit: edits(path).map(|edits| edits.video),
        rebase: None,
        end,
        ended: false,
    };
    let first = reader.read_video()?;
    reader.video.as_mut().expect("video").first = first;
    Ok(reader)
}

/// The rows' stride of an uncompressed picture in a plain buffer: the
/// type's own, else Media Foundation's default for its format.
fn default_stride(media_type: &IMFMediaType, width: u32) -> Result<i32> {
    if let Ok(stride) = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
        return Ok(stride as i32);
    }
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }?;
    Ok(unsafe { MFGetStrideForBitmapInfoHeader(subtype.data1, width) }?)
}

/// One `ReadSample`: its flags, time and sample.
struct Read {
    flags: u32,
    time: i64,
    sample: Option<IMFSample>,
}

impl Reader {
    fn read(&mut self) -> Result<Read> {
        let mut flags = 0;
        let mut time = 0;
        let mut sample = None;
        unsafe {
            self.reader.ReadSample(
                stream(self.part),
                0,
                None,
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            )
        }
        .map_err(|error| anyhow!("could not read {}: {}", self.path, reason(&error)))?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            bail!("could not read {}: the decoder failed", self.path);
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            self.ended = true;
        }
        Ok(Read {
            flags,
            time,
            sample,
        })
    }

    fn read_video(&mut self) -> Result<Option<(Frame, Duration)>> {
        while !self.ended {
            let read = self.read()?;
            if read.flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                self.relayout()?;
            }
            let Some(sample) = read.sample else {
                continue;
            };
            let time = self.presentation(read.time);
            // A picture before the file's start (an edit list's lead-in)
            // isn't shown; one that is still showing at the start is shown
            // from it.
            if time < 0 {
                let duration = unsafe { sample.GetSampleDuration() }.unwrap_or(0);
                if time.saturating_add(duration) <= 0 {
                    continue;
                }
            }
            let at = from_hns(time, &self.path)?;
            if self.end.is_some_and(|end| at >= end) {
                continue;
            }
            let video = self.video.as_mut().expect("video reader");
            let frame = if video.gpu {
                video.copy(sample).inspect_err(|_| {
                    // Plays after this one decode it on the CPU.
                    decode_on_cpu(&self.file);
                })?
            } else {
                video.upload(&sample)?
            };
            return Ok(Some((frame, at)));
        }
        Ok(None)
    }

    /// A sample's time from the file's start. Where Media Foundation
    /// applied a QuickTime/MP4 file's edit list, the first picture comes at
    /// the edit's delay, the first sound between the delay less the
    /// skipped priming and the delay, and times stand. Where it didn't
    /// (Windows 10, Server 2022), the first picture is the first shown and
    /// the first sound is the media's start, priming included, one frame
    /// late: both are put where the edit says.
    fn presentation(&mut self, raw: i64) -> i64 {
        let Some(edit) = self.edit else {
            return raw;
        };
        let part = self.part;
        let rebase = *self.rebase.get_or_insert_with(|| {
            let (delay, skip) = (hns(edit.delay), hns(edit.skip));
            let (applied, skip) = match part {
                MediaPart::Video => (raw.abs_diff(delay) <= ALIGNMENT_HNS as u64, 0),
                MediaPart::Audio => (
                    (delay.saturating_sub(skip).saturating_sub(ALIGNMENT_HNS)
                        ..=delay.saturating_add(ALIGNMENT_HNS))
                        .contains(&raw),
                    skip,
                ),
            };
            if applied {
                0
            } else {
                raw.saturating_add(skip).saturating_sub(delay)
            }
        });
        raw.saturating_sub(rebase)
    }

    /// Where this part's media starts showing: sound before it (priming,
    /// put before an empty edit's end) isn't played.
    fn starts_at(&self) -> i64 {
        self.edit.map_or(0, |edit| hns(edit.delay))
    }

    /// The decoder changed the picture's size or layout mid-file.
    fn relayout(&mut self) -> Result<()> {
        let index = stream(self.part);
        let current = unsafe { self.reader.GetCurrentMediaType(index) }?;
        let native = unsafe { self.reader.GetNativeMediaType(index, 0) }?;
        let (frame, size) = layout(&native, &current, &self.path)?;
        let video = self.video.as_mut().expect("video reader");
        video.size = size;
        video.stride = default_stride(&current, frame.0)?;
        Ok(())
    }

    /// The next sound, one chunk behind the decoder: a chunk the next one
    /// starts inside of loses what it overlaps. Media Foundation's
    /// resampler stamps the first chunk it makes (an AAC encoder's priming
    /// samples, which the file's edit list puts before its start) at the
    /// start, where the file's real sound then starts too.
    fn read_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        loop {
            let next = self.read_chunk()?;
            let Some((mut samples, at)) = self.ahead.take() else {
                match next {
                    Some(next) => {
                        self.ahead = Some(next);
                        continue;
                    }
                    None => return Ok(None),
                }
            };
            if let Some((_, next_at)) = &next {
                let end = after(at, frames_duration(samples.len() / 2), &self.path)?;
                if next_at.saturating_add(ALIGNMENT) < end {
                    let keep = next_at.saturating_sub(at).as_nanos() * u128::from(SAMPLE_RATE)
                        / 1_000_000_000;
                    samples.truncate(keep as usize * 2);
                }
            }
            self.ahead = next;
            if samples.is_empty() {
                continue;
            }
            let mut at = at;
            if let Some(end) = self.audio_end
                && at.abs_diff(end) <= ALIGNMENT
            {
                at = end;
            }
            self.audio_end = Some(after(at, frames_duration(samples.len() / 2), &self.path)?);
            return Ok(Some((samples, at)));
        }
    }

    /// The next chunk of sound as decoded, with its time.
    fn read_chunk(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        while !self.ended {
            let read = self.read()?;
            if read.flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                let current = unsafe { self.reader.GetCurrentMediaType(stream(self.part)) }?;
                check_audio(&current, &self.path)?;
            }
            let Some(sample) = read.sample else {
                continue;
            };
            let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
            let mut data = std::ptr::null_mut();
            let mut length = 0;
            unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }?;
            let samples = if data.is_null() {
                Vec::new()
            } else {
                // Whole stereo frames only.
                let count = length as usize / 8 * 2;
                let mut samples = vec![0f32; count];
                // SAFETY: the locked buffer holds `length` bytes, and
                // float samples need no alignment in a byte copy.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.cast_const(),
                        samples.as_mut_ptr().cast::<u8>(),
                        count * 4,
                    )
                };
                samples
            };
            unsafe { buffer.Unlock() }?;
            // Sound before the file's start (an AAC encoder's priming
            // samples, which its edit list cuts) isn't played; a chunk
            // that crosses the start plays from it.
            let mut samples = samples;
            if samples.is_empty() {
                continue;
            }
            let mut time = self.presentation(read.time);
            let start = self.starts_at();
            if time < start {
                let early = (u128::from(start.abs_diff(time)) * u128::from(SAMPLE_RATE))
                    .div_ceil(10_000_000);
                let early = usize::try_from(early).unwrap_or(usize::MAX);
                if early >= samples.len() / 2 {
                    continue;
                }
                samples.drain(..early * 2);
                time = start;
            }
            let at = from_hns(time, &self.path)?;
            if let Some(end) = self.end {
                let frames =
                    end.saturating_sub(at).as_nanos() * u128::from(SAMPLE_RATE) / 1_000_000_000;
                samples.truncate(
                    usize::try_from(frames)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(2),
                );
                if samples.is_empty() {
                    continue;
                }
            }
            return Ok(Some((samples, at)));
        }
        Ok(None)
    }
}

impl Video {
    /// Drop GPU samples whose pool copy has finished reading them.
    fn release(&mut self) -> Result<()> {
        let completed = self.device.completed()?;
        while self
            .retained
            .front()
            .is_some_and(|(_, frame)| frame.ready_value() <= completed)
        {
            self.retained.pop_front();
        }
        Ok(())
    }

    /// Copy a GPU-decoded picture into the pool.
    fn copy(&mut self, sample: IMFSample) -> Result<Frame> {
        self.release()?;
        let buffer = unsafe { sample.GetBufferByIndex(0) }?;
        let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else {
            // A decoder may give some pictures in system memory.
            return self.upload(&sample);
        };
        let mut raw = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }?;
        // SAFETY: GetResource returned an owned reference of that interface.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let (width, height) = (self.size.0.min(desc.Width), self.size.1.min(desc.Height));
        let frame = self
            .pool
            .copy_subresource(&texture, subresource, width, height)?
            .context("media.frame: all decoded pictures are still in use")?;
        // Keep the decoder's texture until the copy has read it.
        self.retained.push_back((sample, frame.clone()));
        Ok(frame)
    }

    /// Upload a CPU-decoded picture, its alpha made opaque (RGB32 leaves
    /// that byte undefined).
    fn upload(&mut self, sample: &IMFSample) -> Result<Frame> {
        let (width, height) = self.size;
        let row = width as usize * 4;
        self.pixels.resize(row * height as usize, 0);
        let buffer = match unsafe { sample.GetBufferCount() }? {
            1 => unsafe { sample.GetBufferByIndex(0) }?,
            _ => unsafe { sample.ConvertToContiguousBuffer() }?,
        };
        // A 2D buffer says where its rows really are; a plain one is laid
        // out by the type's stride.
        let lock = match buffer.cast::<IMF2DBuffer2>() {
            Ok(two) => Locked::two(two)?,
            Err(_) => Locked::plain(buffer, self.stride)?,
        };
        let length = lock.length as usize;
        // The first row's offset in the buffer, then each row's from it.
        let first = (lock.scan0 as isize).checked_sub(lock.start as isize);
        for (y, out) in self.pixels.chunks_exact_mut(row).enumerate() {
            let offset = first
                .and_then(|first| first.checked_add((y as isize).checked_mul(lock.pitch as isize)?))
                .and_then(|offset| usize::try_from(offset).ok())
                .filter(|offset| {
                    !lock.start.is_null()
                        && offset.checked_add(row).is_some_and(|end| end <= length)
                });
            let Some(offset) = offset else {
                bail!(
                    "media.frame: row {y} of a {width}x{height} picture lies outside its {length}-byte buffer (pitch {})",
                    lock.pitch
                );
            };
            // SAFETY: the row lies within the locked buffer, checked above.
            out.copy_from_slice(unsafe { std::slice::from_raw_parts(lock.start.add(offset), row) });
            for pixel in out.as_chunks_mut::<4>().0 {
                pixel[3] = 0xff;
            }
        }
        drop(lock);
        self.pool
            .upload_bgra(width, height, &self.pixels, row as u32)?
            .context("media.frame: all decoded pictures are still in use")
    }
}

/// A locked picture buffer: its first row, the step between rows
/// (negative bottom-up), and the bytes it spans. Unlocked on drop.
struct Locked {
    buffer: Result<IMF2DBuffer2, IMFMediaBuffer>,
    scan0: *const u8,
    pitch: i32,
    start: *const u8,
    length: u32,
}

impl Locked {
    fn two(buffer: IMF2DBuffer2) -> Result<Self> {
        let (mut scan0, mut pitch, mut start, mut length) =
            (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
        unsafe {
            buffer.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scan0,
                &mut pitch,
                &mut start,
                &mut length,
            )
        }?;
        Ok(Self {
            buffer: Ok(buffer),
            scan0: scan0.cast_const(),
            pitch,
            start: start.cast_const(),
            length,
        })
    }

    fn plain(buffer: IMFMediaBuffer, stride: i32) -> Result<Self> {
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }?;
        let rows = if stride == 0 {
            0
        } else {
            length / stride.unsigned_abs()
        };
        let scan0 = if stride < 0 && rows > 0 {
            data.wrapping_add((rows as usize - 1) * stride.unsigned_abs() as usize)
        } else {
            data
        };
        Ok(Self {
            buffer: Err(buffer),
            scan0: scan0.cast_const(),
            pitch: stride,
            start: data.cast_const(),
            length,
        })
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = match &self.buffer {
            Ok(two) => unsafe { two.Unlock2D() },
            Err(plain) => unsafe { plain.Unlock() },
        };
    }
}

impl isocast_engine::MediaReader for Reader {
    type Frame = Frame;

    fn next_video(&mut self) -> Result<Option<(Frame, Duration)>> {
        if self.part != MediaPart::Video {
            bail!("{} was opened for its audio", self.path);
        }
        if let Some(first) = self.video.as_mut().and_then(|video| video.first.take()) {
            return Ok(Some(first));
        }
        self.read_video()
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        if self.part != MediaPart::Audio {
            bail!("{} was opened for its video", self.path);
        }
        self.read_audio()
    }
}

#[cfg(test)]
mod tests;
