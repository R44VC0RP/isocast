//! Real Media Foundation decoding of files ffmpeg makes at test time
//! (ffmpeg must be on PATH, as in the Windows CI job).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use isocast_engine::MediaReader as _;
use windows::Win32::Graphics::Direct3D11::*;

use super::*;

/// A folder of fixtures, removed when the test ends.
struct Fixtures(PathBuf);

impl Fixtures {
    fn new(test: &str) -> Self {
        let folder =
            std::env::temp_dir().join(format!("isocast-media-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        Self(folder)
    }

    /// Run ffmpeg to make `name`; `None` if this ffmpeg can't (a missing
    /// encoder), with its reason printed.
    fn make(&self, name: &str, args: &[&str]) -> Option<PathBuf> {
        let path = self.0.join(name);
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .arg(&path)
            .output()
            .expect("ffmpeg must be on PATH to make the media fixtures");
        if !out.status.success() {
            eprintln!(
                "ffmpeg could not make {name}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Some(path)
    }

    fn require(&self, name: &str, args: &[&str]) -> PathBuf {
        self.make(name, args)
            .unwrap_or_else(|| panic!("ffmpeg could not make {name}"))
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 2 s of red 320x240 at 30 fps with B-frames, and a 440 Hz tone at
/// 44.1 kHz whose right channel is at half level, as H.264/AAC MP4.
fn h264_aac(fixtures: &Fixtures) -> PathBuf {
    fixtures.require(
        "red.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=320x240:r=30:d=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=2",
            "-filter_complex",
            "[1:a]pan=stereo|c0=c0|c1=0.5*c0[a]",
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
        ],
    )
}

fn device() -> GraphicsDevice {
    GraphicsDevice::new().expect("a D3D11 device (WARP on hosted runners)")
}

/// The BGRA pixel at (x, y) of a decoded picture, read back from the GPU.
fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    assert!(frame.wait_ready(Duration::from_secs(5)).unwrap());
    unsafe {
        let device = frame.texture().GetDevice().unwrap();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        frame.texture().GetDesc(&mut desc);
        assert_eq!(
            desc.Format,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM
        );
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .unwrap();
        let staging = staging.unwrap();
        let context = device.GetImmediateContext().unwrap();
        context.CopyResource(&staging, frame.texture());
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .unwrap();
        let at = mapped
            .pData
            .cast::<u8>()
            .add(y as usize * mapped.RowPitch as usize + x as usize * 4);
        let value = [*at, *at.add(1), *at.add(2), *at.add(3)];
        context.Unmap(&staging, 0);
        value
    }
}

/// Every picture of a reader: its size, time and centre pixel.
fn pictures(reader: &mut Reader) -> Vec<((u32, u32), Duration, [u8; 4])> {
    let mut out = Vec::new();
    while let Some((frame, at)) = reader.next_video().unwrap() {
        let colour = pixel(&frame, frame.width() / 2, frame.height() / 2);
        out.push(((frame.width(), frame.height()), at, colour));
    }
    out
}

/// All the sound: interleaved samples and each chunk's (time, frames).
fn sound(reader: &mut Reader) -> (Vec<f32>, Vec<(Duration, usize)>) {
    let (mut samples, mut chunks) = (Vec::new(), Vec::new());
    while let Some((chunk, at)) = reader.next_audio().unwrap() {
        chunks.push((at, chunk.len() / 2));
        samples.extend(chunk);
    }
    (samples, chunks)
}

fn rms(samples: &[f32], channel: usize) -> f64 {
    let values: Vec<f64> = samples
        .iter()
        .skip(channel)
        .step_by(2)
        .map(|v| f64::from(*v))
        .collect();
    (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt()
}

/// Each chunk starts where the one before ended (within one sample).
fn assert_contiguous(chunks: &[(Duration, usize)]) {
    for pair in chunks.windows(2) {
        let ((at, frames), (next, _)) = (pair[0], pair[1]);
        let end = at + Duration::from_nanos(frames as u64 * 1_000_000_000 / 48_000);
        assert!(
            next.abs_diff(end) <= Duration::from_micros(21),
            "sound jumps from {end:?} to {next:?}"
        );
    }
}

fn assert_red(colour: [u8; 4]) {
    let [b, g, r, a] = colour;
    assert!(
        r > 200 && g < 60 && b < 60 && a == 255,
        "expected opaque red BGRA, got {colour:?}"
    );
}

fn assert_red_h264_pictures(reader: &mut Reader) {
    let pictures = pictures(reader);
    assert_eq!(pictures.len(), 60, "2 s at 30 fps");
    for (n, (size, at, colour)) in pictures.iter().enumerate() {
        assert_eq!(*size, (320, 240));
        let expected = Duration::from_secs_f64(n as f64 / 30.0);
        assert!(
            at.abs_diff(expected) <= Duration::from_millis(2),
            "picture {n} at {at:?}, expected {expected:?}"
        );
        assert_red(*colour);
    }
    // The end stays the end.
    assert!(reader.next_video().unwrap().is_none());
}

#[test]
fn h264_aac_mp4_plays() {
    let fixtures = Fixtures::new("h264");
    let file = h264_aac(&fixtures);
    let info = media_info(&file).unwrap();
    assert_eq!(info.video, Some((320, 240)));
    assert!(info.audio);
    assert!(
        info.duration.abs_diff(Duration::from_secs(2)) <= Duration::from_millis(100),
        "duration {:?}",
        info.duration
    );

    let device = device();
    // As the engine opens it: the GPU path where it opens, else the CPU.
    assert_red_h264_pictures(&mut open(Some(device.clone()), &file, MediaPart::Video).unwrap());
    // Both paths, where this machine has them, decode the same pictures.
    assert_red_h264_pictures(
        &mut open_video(&device, &file, &file.display().to_string(), false).unwrap(),
    );
    match open_video(&device, &file, &file.display().to_string(), true) {
        Ok(mut reader) => assert_red_h264_pictures(&mut reader),
        Err(error) => eprintln!("no GPU decoding on this machine: {error:#}"),
    }

    let mut reader = open(None, &file, MediaPart::Audio).unwrap();
    let (samples, chunks) = sound(&mut reader);
    eprintln!(
        "first sound chunks (time, frames): {:?}",
        &chunks[..chunks.len().min(4)]
    );
    assert!(
        chunks[0].0 <= Duration::from_millis(30),
        "sound starts at {:?}",
        chunks[0].0
    );
    assert_contiguous(&chunks);
    let frames = samples.len() / 2;
    assert!(
        frames.abs_diff(96_000) <= 3_000,
        "{frames} frames at 48 kHz for 2 s"
    );
    // The channels stay apart: right at half the left's level.
    let ratio = rms(&samples, 1) / rms(&samples, 0);
    assert!((ratio - 0.5).abs() < 0.05, "right/left level {ratio}");
    assert!(reader.next_audio().unwrap().is_none());
}

#[test]
fn mono_wav_becomes_48k_stereo() {
    let fixtures = Fixtures::new("wav");
    // ffmpeg's sine has a peak of 1/8.
    let file = fixtures.require(
        "tone.wav",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=22050:duration=1",
            "-c:a",
            "pcm_s16le",
        ],
    );
    let info = media_info(&file).unwrap();
    assert_eq!(info.video, None);
    assert!(info.audio);
    assert!(info.duration.abs_diff(Duration::from_secs(1)) <= Duration::from_millis(20));

    let mut reader = open(None, &file, MediaPart::Audio).unwrap();
    let (samples, chunks) = sound(&mut reader);
    assert_eq!(chunks[0].0, Duration::ZERO);
    assert_contiguous(&chunks);
    let frames = samples.len() / 2;
    assert!(frames.abs_diff(48_000) <= 100, "{frames} frames");
    assert!(
        samples.as_chunks::<2>().0.iter().all(|[l, r]| l == r),
        "mono is the same on both sides"
    );
    let peak = samples.iter().fold(0f32, |peak, v| peak.max(v.abs()));
    assert!((peak - 0.125).abs() < 0.01, "peak {peak}");

    let error = open(Some(device()), &file, MediaPart::Video).err().unwrap();
    assert!(format!("{error:#}").contains("has no video"), "{error:#}");
}

#[test]
fn a_codec_without_a_decoder_is_named() {
    let fixtures = Fixtures::new("ffv1");
    // No Windows has an FFV1 decoder.
    let file = fixtures.require(
        "lossless.avi",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=s=160x120:r=10:d=1",
            "-c:v",
            "ffv1",
        ],
    );
    for error in [
        media_info(&file).err().unwrap(),
        open(Some(device()), &file, MediaPart::Video).err().unwrap(),
    ] {
        let text = format!("{error:#}");
        assert!(text.contains(&file.display().to_string()), "{text}");
        assert!(text.contains("no decoder for its FFV1 video"), "{text}");
        assert!(text.contains("convert the file"), "{text}");
    }
}

/// Codecs Windows decodes only with a Store extension: they play where it
/// is installed, and otherwise the error names the codec and the extension.
#[test]
fn store_codecs_play_or_name_their_extension() {
    let fixtures = Fixtures::new("store");
    let cases = [
        (
            "hevc.mp4",
            vec![
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=25:d=1",
                "-c:v",
                "libx265",
                "-pix_fmt",
                "yuv420p",
                "-tag:v",
                "hvc1",
            ],
            "HEVC",
            "HEVC Video Extensions",
        ),
        (
            "vp9.webm",
            vec![
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=25:d=1",
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuv420p",
            ],
            "VP9",
            "Microsoft Store",
        ),
        (
            "av1.mp4",
            vec![
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:r=25:d=1",
                "-c:v",
                "libaom-av1",
                "-cpu-used",
                "8",
                "-pix_fmt",
                "yuv420p",
            ],
            "AV1",
            "AV1 Video Extension",
        ),
    ];
    let device = device();
    for (name, args, codec, extension) in cases {
        let Some(file) = fixtures.make(name, &args) else {
            continue;
        };
        match media_info(&file).and_then(|_| open(Some(device.clone()), &file, MediaPart::Video)) {
            Ok(mut reader) => {
                let pictures = pictures(&mut reader);
                assert_eq!(pictures.len(), 25, "{name}");
                let [b, g, r, a] = pictures[0].2;
                assert!(
                    b > 200 && g < 60 && r < 60 && a == 255,
                    "{name}: {:?}",
                    pictures[0].2
                );
                eprintln!("{name}: Windows decodes {codec} here");
            }
            Err(error) => {
                let text = format!("{error:#}");
                assert!(
                    text.contains(codec) && text.contains(extension),
                    "{name}: {text}"
                );
                eprintln!("{name}: {text}");
            }
        }
    }
}

#[test]
fn unreadable_files_say_why() {
    let fixtures = Fixtures::new("bad");
    let missing = fixtures.0.join("missing.mp4");
    let error = format!("{:#}", media_info(&missing).err().unwrap());
    assert!(error.contains("does not exist"), "{error}");
    let error = format!(
        "{:#}",
        open(None, &missing, MediaPart::Audio).err().unwrap()
    );
    assert!(error.contains("does not exist"), "{error}");

    let garbage = fixtures.0.join("garbage.mp4");
    std::fs::write(
        &garbage,
        (0..65536u32)
            .map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let error = format!("{:#}", media_info(&garbage).err().unwrap());
    assert!(
        error.contains("could not read") && error.contains("garbage.mp4"),
        "{error}"
    );

    // A video-only file has no audio to open.
    let silent = fixtures.require(
        "silent.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=160x120:r=10:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let info = media_info(&silent).unwrap();
    assert_eq!((info.video, info.audio), (Some((160, 120)), false));
    let error = format!("{:#}", open(None, &silent, MediaPart::Audio).err().unwrap());
    assert!(error.contains("has no audio"), "{error}");
}

#[test]
fn readers_dropped_part_way_release_their_pictures() {
    let fixtures = Fixtures::new("drop");
    let file = h264_aac(&fixtures);
    let device = device();
    let mut held = Vec::new();
    for _ in 0..5 {
        let mut video = open(Some(device.clone()), &file, MediaPart::Video).unwrap();
        let mut audio = open(None, &file, MediaPart::Audio).unwrap();
        for _ in 0..3 {
            held.push(video.next_video().unwrap().unwrap().0);
            audio.next_audio().unwrap().unwrap();
        }
    }
    // Pictures outlive the reader that decoded them.
    for frame in &held {
        assert_red(pixel(frame, 8, 8));
    }
}

#[test]
fn codec_names_and_fixes() {
    assert!(codec(MFVideoFormat_HEVC).fix.contains("9NMZLZ57R3T7"));
    assert_eq!(codec(MFVideoFormat_H264).name, "H.264");
    let ffv1 = GUID::from_u128(0x31564646_0000_0010_8000_00aa00389b71);
    assert_eq!(codec(ffv1).name, "FFV1");
    assert_eq!(codec(ffv1).fix, CONVERT);
    let odd = GUID::from_u128(0x01020304_0506_0708_090a_0b0c0d0e0f10);
    assert!(codec(odd).name.contains("01020304"));
}

/// Diagnostic, run by hand: every ReadSample of a file's pictures on each
/// decode path (flags, time, duration), as Media Foundation gives them.
#[test]
#[ignore]
fn trace_video_reads() {
    let fixtures = Fixtures::new("trace");
    let mut files = vec![h264_aac(&fixtures)];
    files.extend(fixtures.make(
        "vp9.webm",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x240:r=25:d=1",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuv420p",
        ],
    ));
    files.extend(fixtures.make(
        "vp9-noaltref.webm",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x240:r=25:d=1",
            "-c:v",
            "libvpx-vp9",
            "-auto-alt-ref",
            "0",
            "-lag-in-frames",
            "0",
            "-pix_fmt",
            "yuv420p",
        ],
    ));
    let device = device();
    start().unwrap();
    for file in files {
        // GPU and CPU decoding, the decoder alone (NV12, no video
        // processor), and the source's own compressed samples.
        for mode in ["gpu", "cpu", "decoder", "compressed"] {
            let shown = file.display().to_string();
            let gpu = mode == "gpu";
            let attributes = if mode == "decoder" {
                attributes(1).unwrap()
            } else {
                video_attributes(&device, gpu).unwrap()
            };
            let reader = source_reader(&file, &shown, &attributes).unwrap();
            select(&reader, MediaPart::Video, &shown).unwrap();
            let rate = |media_type: &IMFMediaType| {
                unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }
                    .map(|rate| format!("{}/{}", rate >> 32, rate as u32))
                    .unwrap_or_else(|_| "-".into())
            };
            let native = unsafe { reader.GetNativeMediaType(stream(MediaPart::Video), 0) }.unwrap();
            let configured = match mode {
                "compressed" => Ok(()),
                "decoder" => (|| {
                    let wanted = unsafe { MFCreateMediaType() }?;
                    unsafe {
                        wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                        wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                        reader.SetCurrentMediaType(stream(MediaPart::Video), None, &wanted)?;
                    }
                    anyhow::Ok(())
                })(),
                _ => set_output(&reader, MediaPart::Video, &shown, gpu).map(|_| ()),
            };
            if let Err(error) = configured {
                eprintln!("{shown} {mode}: {error:#}");
                continue;
            }
            let current = unsafe { reader.GetCurrentMediaType(stream(MediaPart::Video)) }.unwrap();
            eprintln!(
                "{shown} {mode}: native rate {} current rate {} subtype {:?}",
                rate(&native),
                rate(&current),
                unsafe { current.GetGUID(&MF_MT_SUBTYPE) }.ok()
            );
            let mut reads = Vec::new();
            for _ in 0..200 {
                let (mut flags, mut time, mut sample) = (0, 0, None);
                unsafe {
                    reader.ReadSample(
                        stream(MediaPart::Video),
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut time),
                        Some(&mut sample),
                    )
                }
                .unwrap();
                let duration = sample
                    .as_ref()
                    .and_then(|s: &IMFSample| unsafe { s.GetSampleDuration() }.ok());
                reads.push(format!(
                    "{flags:#x}:{:.1}ms{}",
                    time as f64 / 1e4,
                    duration.map_or("/-".into(), |d| format!("/{:.1}", d as f64 / 1e4))
                ));
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
            eprintln!("  {} reads: {}", reads.len(), reads.join(" "));
        }
    }
}

/// Diagnostic, run by hand: the sound's ReadSample times, resampled to
/// 48 kHz stereo and at the file's own rate.
#[test]
#[ignore]
fn trace_audio_reads() {
    let fixtures = Fixtures::new("trace-audio");
    let file = h264_aac(&fixtures);
    start().unwrap();
    let shown = file.display().to_string();
    for resample in [true, false] {
        let reader = source_reader(&file, &shown, &attributes(1).unwrap()).unwrap();
        select(&reader, MediaPart::Audio, &shown).unwrap();
        if resample {
            set_output(&reader, MediaPart::Audio, &shown, false).unwrap();
        } else {
            let wanted = unsafe { MFCreateMediaType() }.unwrap();
            unsafe {
                wanted
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                    .unwrap();
                wanted
                    .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
                    .unwrap();
                reader
                    .SetCurrentMediaType(stream(MediaPart::Audio), None, &wanted)
                    .unwrap();
            }
        }
        let current = unsafe { reader.GetCurrentMediaType(stream(MediaPart::Audio)) }.unwrap();
        let rate = unsafe { current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }.unwrap();
        let channels = unsafe { current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }.unwrap();
        let mut reads = Vec::new();
        for _ in 0..12 {
            let (mut flags, mut time, mut sample) = (0, 0, None);
            unsafe {
                reader.ReadSample(
                    stream(MediaPart::Audio),
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )
            }
            .unwrap();
            let frames = sample.as_ref().map_or(0, |s: &IMFSample| {
                unsafe { s.GetTotalLength() }.unwrap_or(0) / 4 / channels
            });
            reads.push(format!("{flags:#x}:{:.2}ms+{frames}", time as f64 / 1e4));
        }
        eprintln!(
            "audio resample={resample} {rate} Hz {channels} ch: {}",
            reads.join(" ")
        );
    }
}

/// Where a file's content lands in time: a white picture and a sound burst
/// both at exactly 1.5 s (frame 45 of 30 fps), in MP4 and MOV, with and
/// without B-frames, at 44.1 and 48 kHz. Edit lists, encoder priming and
/// decoder delay must not move either: the engine plays them in step.
#[test]
fn pictures_and_sound_land_on_time() {
    let fixtures = Fixtures::new("timing");
    let device = device();
    let mut failures = Vec::new();
    // (file, picture offset, sound offset, B-frames, sample rate): an
    // offset input starts late in the file (ffmpeg's -itsoffset writes an
    // empty edit), its content moved to keep both marks at 1.5 s.
    let cases: [(&str, f64, f64, &str, &str); 5] = [
        ("bf2-44k.mp4", 0.0, 0.0, "2", "44100"),
        ("bf0-48k.mp4", 0.0, 0.0, "0", "48000"),
        ("bf2-48k.mov", 0.0, 0.0, "2", "48000"),
        ("late-sound.mp4", 0.0, 0.5, "2", "48000"),
        ("late-picture.mp4", 0.5, 0.0, "2", "48000"),
    ];
    for (name, picture_offset, sound_offset, bframes, rate) in cases {
        let (picture_offset, sound_offset) = (picture_offset.to_string(), sound_offset.to_string());
        let video_input = format!(
            "color=c=black:s=160x120:r=30:d={},drawbox=enable='eq(n,{})':c=white:t=fill",
            3.0 - picture_offset.parse::<f64>().unwrap(),
            45 - (picture_offset.parse::<f64>().unwrap() * 30.0) as u32
        );
        let sound_at = 1.5 - sound_offset.parse::<f64>().unwrap();
        let audio_input = format!(
            "aevalsrc='0.5*sin(2*PI*1000*t)*between(t,{sound_at},{})':s={rate}:d={}",
            sound_at + 0.02,
            3.0 - sound_offset.parse::<f64>().unwrap()
        );
        let mut args = vec![
            "-itsoffset",
            &picture_offset,
            "-f",
            "lavfi",
            "-i",
            &video_input,
        ];
        args.extend([
            "-itsoffset",
            &sound_offset,
            "-f",
            "lavfi",
            "-i",
            &audio_input,
        ]);
        args.extend([
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-bf", bframes, "-c:a", "aac", "-b:a", "128k",
        ]);
        let file = fixtures.require(name, &args);
        let mut video = open(Some(device.clone()), &file, MediaPart::Video).unwrap();
        let mut white = None;
        let mut first = None;
        while let Some((frame, at)) = video.next_video().unwrap() {
            first.get_or_insert(at);
            if pixel(&frame, 80, 60)[1] > 128 {
                white.get_or_insert(at);
            }
        }
        let mut audio = open(None, &file, MediaPart::Audio).unwrap();
        let (mut onset, mut first_sound, mut sound_end) = (None, None, Duration::ZERO);
        while let Some((samples, at)) = audio.next_audio().unwrap() {
            first_sound.get_or_insert(at);
            sound_end =
                at + Duration::from_nanos((samples.len() / 2) as u64 * 1_000_000_000 / 48_000);
            if onset.is_none()
                && let Some(i) = samples.iter().step_by(2).position(|v| v.abs() > 0.1)
            {
                onset = Some(at + Duration::from_nanos(i as u64 * 1_000_000_000 / 48_000));
            }
        }
        let at = Duration::from_millis(1500);
        eprintln!(
            "{name}: first picture {first:?}, white {white:?}; first sound {first_sound:?}, burst {onset:?}"
        );
        // A late part starts where its edit says, not at the file's start.
        let late = |offset: &str| Duration::from_secs_f64(offset.parse::<f64>().unwrap());
        if first
            .is_none_or(|first| first.abs_diff(late(&picture_offset)) > Duration::from_millis(2))
        {
            failures.push(format!(
                "{name}: first picture at {first:?}, not {picture_offset} s"
            ));
        }
        // Sound may start up to an AAC frame early (ffmpeg keeps the
        // priming of a late track inside its empty edit).
        if first_sound.is_none_or(|first| {
            first > late(&sound_offset) + Duration::from_millis(2)
                || first + Duration::from_millis(25) < late(&sound_offset)
        }) {
            failures.push(format!(
                "{name}: first sound at {first_sound:?}, not {sound_offset} s"
            ));
        }
        if white.is_none_or(|white| white.abs_diff(at) > Duration::from_millis(2)) {
            failures.push(format!("{name}: white picture at {white:?}, not 1.5 s"));
        }
        if onset.is_none_or(|onset| onset.abs_diff(at) > Duration::from_millis(3)) {
            failures.push(format!("{name}: burst at {onset:?}, not 1.5 s"));
        }
        // The sound lasts as long as the file: the next loop starts there.
        let length = media_info(&file).unwrap().duration;
        eprintln!("{name}: length {length:?}, sound ends {sound_end:?}");
        if sound_end > length + Duration::from_micros(21)
            || sound_end + Duration::from_millis(25) < length
        {
            failures.push(format!(
                "{name}: sound ends at {sound_end:?}, the file at {length:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn edit_lists_are_read_and_bad_headers_refused() {
    let fixtures = Fixtures::new("edits");
    let file = h264_aac(&fixtures);
    let read = isocast_protocol::mp4_edits::read(&file).expect("an MP4's edits");
    // ffmpeg's x264 with two B-frames starts showing at its third media
    // frame's time (1024 of 15360); its AAC skips 1024 priming samples.
    assert_eq!(read.video.delay, Duration::ZERO);
    assert_eq!(
        read.video.skip,
        Duration::from_nanos(1024 * 1_000_000_000 / 15360)
    );
    assert_eq!(
        read.audio.skip,
        Duration::from_nanos(1024 * 1_000_000_000 / 44100)
    );
    let wav = fixtures.require(
        "tone.wav",
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=duration=0.1",
            "-c:a",
            "pcm_s16le",
        ],
    );
    assert_eq!(isocast_protocol::mp4_edits::read(&wav), None);

    // Every truncation and many corruptions of a real file are refused or
    // read, never a panic or a runaway read.
    let bytes = std::fs::read(&file).unwrap();
    let damaged = fixtures.0.join("damaged.mp4");
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for round in 0..600usize {
        let mut copy = bytes.clone();
        if round < 300 {
            copy.truncate(bytes.len() * round / 300);
        } else {
            for _ in 0..8 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let at = (seed as usize) % copy.len();
                copy[at] = (seed >> 32) as u8;
            }
        }
        std::fs::write(&damaged, &copy).unwrap();
        let _ = isocast_protocol::mp4_edits::read(&damaged);
    }
}

/// Diagnostic, run by hand with ISOCAST_TRACE_MEDIA=<file>: every chunk of
/// its sound as the engine gets it, where it doesn't continue the last.
#[test]
#[ignore]
fn trace_sound_of_a_file() {
    let Ok(file) = std::env::var("ISOCAST_TRACE_MEDIA") else {
        return;
    };
    let mut reader = open(None, std::path::Path::new(&file), MediaPart::Audio).unwrap();
    let (mut end, mut count) = (None::<Duration>, 0);
    while let Some((samples, at)) = reader.next_audio().unwrap() {
        let frames = samples.len() / 2;
        if count < 3 || end.is_some_and(|end| end != at) || frames != 1024 {
            eprintln!("chunk {count} at {at:?} +{frames} (previous end {end:?})");
        }
        end = Some(at + Duration::from_nanos(frames as u64 * 1_000_000_000 / 48_000));
        count += 1;
    }
    eprintln!("{count} chunks, end {end:?}");
}

/// Diagnostic, run by hand: a 4K H.264 file decoded on each path, with the
/// decoder transforms the reader chose and the process CPU time it took
/// (hardware decoding shows as far less CPU on the GPU path).
#[test]
#[ignore]
fn trace_decoders() {
    let fixtures = Fixtures::new("decoders");
    let file = fixtures.require(
        "uhd.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=3840x2160:r=30:d=4",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let device = device();
    start().unwrap();
    let shown = file.display().to_string();
    for gpu in [true, false] {
        let mut reader = open_video(&device, &file, &shown, gpu).unwrap();
        let mut names = Vec::new();
        if let Ok(ex) = reader.reader.cast::<IMFSourceReaderEx>() {
            for index in 0..8 {
                let (mut category, mut transform) = (GUID::zeroed(), None);
                if unsafe {
                    ex.GetTransformForStream(
                        stream(MediaPart::Video),
                        index,
                        Some(&mut category),
                        &mut transform,
                    )
                }
                .is_err()
                {
                    break;
                }
                let attributes = transform.and_then(|t| unsafe { t.GetAttributes() }.ok());
                let aware = attributes
                    .as_ref()
                    .and_then(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.ok());
                names.push(format!("{category:?} d3d11_aware={aware:?}"));
            }
        }
        let (cpu, wall) = (
            crate::system::process_cpu_seconds().unwrap_or(0.0),
            std::time::Instant::now(),
        );
        let mut frames = 0;
        while let Some((frame, _)) = reader.next_video().unwrap() {
            frame.wait_ready(Duration::from_secs(5)).unwrap();
            frames += 1;
        }
        eprintln!(
            "gpu={gpu}: {frames} 4K pictures in {:.2} s wall, {:.2} s process CPU; transforms {names:?}",
            wall.elapsed().as_secs_f64(),
            crate::system::process_cpu_seconds().unwrap_or(0.0) - cpu
        );
    }
}

/// A slow consumer on a busy GPU (hosted CI's software adapter, where the
/// engine's encoders share the D3D11 queue): pictures are held as the
/// engine's slots hold them while every copy waits behind other GPU work.
/// The reader must neither run Media Foundation's sample allocator dry
/// (MF_E_SAMPLEALLOCATOR_EMPTY ended a hosted take 0.28 s in) nor give up
/// while its pool waits for the GPU.
#[test]
fn a_slow_consumer_on_a_busy_gpu_gets_every_picture() {
    let fixtures = Fixtures::new("busy");
    let file = fixtures.require(
        "busy.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            // Picture n is one flat gray of eight, 27 luma steps apart, so a
            // copy that read a texture the decoder already reused shows.
            "color=c=black:s=1280x720:r=30:d=4,format=yuv420p,geq=lum='16+mod(N\\,8)*27':cb=128:cr=128",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "10",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let device = device();
    // Two large textures to copy back and forth on the shared queue.
    let busy = |_: ()| unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 4096,
            Height: 4096,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        device
            .raw()
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .unwrap();
        texture.unwrap()
    };
    let (a, b) = (busy(()), busy(()));
    start().unwrap();
    let context = unsafe { device.raw().GetImmediateContext() }.unwrap();
    for gpu in [true, false] {
        let check = |n: usize, frame: &Frame| {
            let [b, g, r, _] = pixel(frame, 640, 360);
            let want = (n % 8) as f64 * 27.0 * 255.0 / 219.0;
            assert!(
                [b, g, r]
                    .iter()
                    .all(|v| (f64::from(*v) - want).abs() <= 10.0),
                "gpu={gpu}: picture {n} is {:?}, not gray {want:.0}",
                [b, g, r]
            );
        };
        let mut reader = open_video(&device, &file, &file.display().to_string(), gpu).unwrap();
        let mut held = VecDeque::new();
        let mut count = 0;
        loop {
            for _ in 0..6 {
                unsafe {
                    context.CopyResource(&a, &b);
                    context.CopyResource(&b, &a);
                }
            }
            unsafe { context.Flush() };
            match reader.next_video() {
                Ok(Some((frame, _))) => {
                    held.push_back((count, frame));
                    count += 1;
                    // The engine holds up to five delivered and two
                    // decoded-ahead pictures per source, two readers at a
                    // loop's end: 14 here, more than it ever does. The one
                    // let go is checked after 14 later pictures were
                    // decoded, reusing the decoder's textures.
                    if held.len() > 14 {
                        let (n, frame) = held.pop_front().unwrap();
                        check(n, &frame);
                    }
                }
                Ok(None) => break,
                Err(error) => panic!("gpu={gpu}: picture {count}: {error:#}"),
            }
        }
        for (n, frame) in held {
            check(n, &frame);
        }
        assert_eq!(count, 120, "gpu={gpu}");
        eprintln!("gpu={gpu}: {count} pictures with 14 held on a busy queue");
    }
}
