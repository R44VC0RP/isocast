//! Real Media Foundation decoding of files ffmpeg makes at test time
//! (ffmpeg must be on PATH, as in the Windows CI job).

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
    eprintln!("first sound chunks (time, frames): {:?}", &chunks[..chunks.len().min(4)]);
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
        samples.chunks_exact(2).all(|lr| lr[0] == lr[1]),
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
