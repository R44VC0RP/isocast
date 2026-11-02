//! Decodes fixtures that ffmpeg generates: H.264/AAC MP4, VP9/Opus WebM
//! and a mono WAV, through the system's GStreamer.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use isocast_engine::MediaReader as _;

use super::*;

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Removes the fixture folder when the test process exits.
extern "C" fn remove_fixtures() {
    if let Some(dir) = DIR.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Fixtures made once per test run, in a fresh temporary folder that is
/// removed when the tests finish.
fn fixtures() -> &'static Path {
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("isocast-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: registers a plain function without captured state.
        unsafe { libc::atexit(remove_fixtures) };
        let ffmpeg = |args: &[&str], out: &str| {
            let status = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(args)
                .arg(dir.join(out))
                .status()
                .expect("ffmpeg must be installed to make media fixtures");
            assert!(status.success(), "ffmpeg failed making {out}");
        };
        // 2 s of 320x240 at 30 fps, solid red, with 44.1 kHz stereo AAC.
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0xFF0000:s=320x240:r=30:d=2",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=44100:d=2",
                "-filter_complex",
                "[1]pan=stereo|c0=c0|c1=0.5*c0[a]",
                "-map",
                "0:v",
                "-map",
                "[a]",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "15",
                "-bf",
                "2",
                "-c:a",
                "aac",
                "-shortest",
            ],
            "h264-aac.mp4",
        );
        // 1 s of 256x144 at 25 fps, solid blue, with mono Opus.
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0x0000FF:s=256x144:r=25:d=1",
                "-f",
                "lavfi",
                "-i",
                "sine=f=1000:r=48000:d=1",
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "libopus",
                "-ac",
                "1",
                "-shortest",
            ],
            "vp9-opus.webm",
        );
        // 1 s of mono 16-bit sound at 22.05 kHz.
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=22050:d=1",
                "-c:a",
                "pcm_s16le",
                "-ac",
                "1",
            ],
            "mono.wav",
        );
        // Picture only.
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0x00FF00:s=160x120:r=30:d=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            "video-only.mp4",
        );
        // Opus whose second half starts 30 ms late: a real jump in time.
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=48000:d=1",
                "-af",
                "asetpts='if(gte(T,0.5),PTS+0.03/TB,PTS)'",
                "-c:a",
                "libopus",
            ],
            "gap.webm",
        );
        // Elementary streams: no container, so no length to tell.
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0x808080:s=160x120:r=30:d=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "h264",
            ],
            "raw.h264",
        );
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=48000:d=1",
                "-c:a",
                "aac",
                "-f",
                "adts",
            ],
            "raw.aac",
        );
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=48000:d=1",
                "-c:a",
                "libmp3lame",
                "-write_xing",
                "0",
                "-id3v2_version",
                "0",
            ],
            "plain.mp3",
        );
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0x808080:s=160x120:r=30:d=1",
                "-c:v",
                "mjpeg",
                "-f",
                "mjpeg",
            ],
            "raw.mjpeg",
        );
        // Recorded live: WebM written to a pipe stores no length or index.
        let live = |args: &[&str], out: &str| {
            let mut child = Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(args)
                .args(["-f", "webm", "-"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("ffmpeg must be installed to make media fixtures");
            let mut file = std::fs::File::create(dir.join(out)).unwrap();
            std::io::copy(child.stdout.as_mut().unwrap(), &mut file).unwrap();
            assert!(
                child.wait().unwrap().success(),
                "ffmpeg failed making {out}"
            );
        };
        live(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0x0000FF:s=256x144:r=30:d=3",
                "-f",
                "lavfi",
                "-i",
                "sine=f=1000:r=48000:d=3",
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "libopus",
            ],
            "live.webm",
        );
        live(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:r=48000:d=1",
                "-c:a",
                "libopus",
            ],
            "live-sound.webm",
        );
        std::fs::write(dir.join("garbage.mp4"), vec![0x5au8; 64 * 1024]).unwrap();
        dir
    })
}

fn fixture(name: &str) -> PathBuf {
    fixtures().join(name)
}

/// Every picture of a file: (width, height, time, centre pixel BGRA).
fn pictures(path: &Path) -> Vec<(u32, u32, Duration, [u8; 4])> {
    let mut reader = open_media(path, MediaPart::Video).unwrap();
    let mut out = Vec::new();
    while let Some((frame, at)) = reader.next_video().unwrap() {
        let (w, h) = (frame.width(), frame.height());
        let pixels = frame.read_bgra().unwrap();
        let centre = ((h / 2 * w + w / 2) * 4) as usize;
        out.push((w, h, at, pixels[centre..centre + 4].try_into().unwrap()));
    }
    // The end stays the end.
    assert!(reader.next_video().unwrap().is_none());
    out
}

/// All of a file's sound and its chunks' times.
fn sound(path: &Path) -> (Vec<f32>, Vec<(Duration, usize)>) {
    let mut reader = open_media(path, MediaPart::Audio).unwrap();
    let (mut samples, mut chunks) = (Vec::new(), Vec::new());
    while let Some((chunk, at)) = reader.next_audio().unwrap() {
        assert_eq!(chunk.len() % 2, 0, "interleaved stereo");
        chunks.push((at, chunk.len() / 2));
        samples.extend(chunk);
    }
    assert!(reader.next_audio().unwrap().is_none());
    (samples, chunks)
}

fn close(a: Duration, b: Duration, within: Duration) -> bool {
    a.abs_diff(b) <= within
}

fn assert_pictures(
    pictures: &[(u32, u32, Duration, [u8; 4])],
    size: (u32, u32),
    fps: u32,
    count: usize,
) {
    assert_eq!(pictures.len(), count, "frame count");
    let period = Duration::from_secs(1) / fps;
    assert!(
        pictures[0].2 < Duration::from_millis(5),
        "first frame at {:?}",
        pictures[0].2
    );
    for (n, (w, h, at, _)) in pictures.iter().enumerate() {
        assert_eq!((*w, *h), size);
        // Presentation order at the file's own rate.
        assert!(
            close(*at, period * n as u32, Duration::from_millis(2)),
            "frame {n} at {at:?}, expected {:?}",
            period * n as u32
        );
    }
}

/// Chunks follow each other without gaps or overlaps, from near 0.
fn assert_contiguous(chunks: &[(Duration, usize)]) {
    assert!(
        chunks[0].0 < Duration::from_millis(30),
        "first audio at {:?}",
        chunks[0].0
    );
    let mut expected = chunks[0].0;
    for (at, frames) in chunks {
        assert!(
            close(*at, expected, Duration::from_micros(500)),
            "audio chunk at {at:?}, expected {expected:?}"
        );
        expected = *at + Duration::from_secs_f64(*frames as f64 / f64::from(SAMPLE_RATE));
    }
}

#[test]
fn h264_aac_mp4() {
    let path = fixture("h264-aac.mp4");
    let info = media_info(&path).unwrap();
    assert_eq!(info.video, Some((320, 240)));
    assert!(info.audio);
    assert!(
        close(
            info.duration,
            Duration::from_secs(2),
            Duration::from_millis(50)
        ),
        "{:?}",
        info.duration
    );

    let pictures = pictures(&path);
    assert_pictures(&pictures, (320, 240), 30, 60);
    for (_, _, _, [b, g, r, a]) in &pictures {
        assert!(
            *r > 230 && *g < 30 && *b < 30 && *a == 255,
            "red, got {:?}",
            (b, g, r, a)
        );
    }

    let (samples, chunks) = sound(&path);
    assert_contiguous(&chunks);
    let frames = samples.len() / 2;
    // 2 s resampled from 44.1 kHz, within an AAC frame.
    assert!(frames.abs_diff(96_000) < 1_200, "{frames} audio frames");
    // The right channel is half the left: channels kept apart.
    let rms = |c: usize| {
        let side: Vec<f32> = samples.iter().skip(c).step_by(2).copied().collect();
        (side.iter().map(|s| s * s).sum::<f32>() / side.len() as f32).sqrt()
    };
    let (left, right) = (rms(0), rms(1));
    assert!(left > 0.05, "left rms {left}");
    assert!(
        (right / left - 0.5).abs() < 0.05,
        "left rms {left}, right rms {right}"
    );
}

#[test]
fn vp9_opus_webm_mono_becomes_stereo() {
    let path = fixture("vp9-opus.webm");
    let info = media_info(&path).unwrap();
    assert_eq!(info.video, Some((256, 144)));
    assert!(info.audio);
    assert!(
        close(
            info.duration,
            Duration::from_secs(1),
            Duration::from_millis(60)
        ),
        "{:?}",
        info.duration
    );

    let pictures = pictures(&path);
    assert_pictures(&pictures, (256, 144), 25, 25);
    let [b, g, r, _] = pictures[0].3;
    assert!(b > 230 && g < 30 && r < 30, "blue, got {:?}", (b, g, r));

    let (samples, chunks) = sound(&path);
    assert_contiguous(&chunks);
    let frames = samples.len() / 2;
    assert!(frames.abs_diff(48_000) < 1_000, "{frames} audio frames");
    for pair in samples.as_chunks::<2>().0 {
        assert_eq!(pair[0], pair[1], "mono plays on both sides");
    }
    assert!(samples.iter().any(|s| s.abs() > 0.05));
}

#[test]
fn wav_resampled_to_48k_stereo() {
    let path = fixture("mono.wav");
    let info = media_info(&path).unwrap();
    assert_eq!(info.video, None);
    assert!(info.audio);
    assert!(close(
        info.duration,
        Duration::from_secs(1),
        Duration::from_millis(5)
    ));

    let (samples, chunks) = sound(&path);
    assert_contiguous(&chunks);
    let frames = samples.len() / 2;
    assert!(frames.abs_diff(48_000) < 100, "{frames} audio frames");
    for pair in samples.as_chunks::<2>().0 {
        assert_eq!(pair[0], pair[1]);
    }
    // ffmpeg's sine is 1/8 of full scale.
    let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!((peak - 0.125).abs() < 0.01, "peak {peak}");

    let err = open_media(&path, MediaPart::Video).err().unwrap();
    assert!(format!("{err:#}").contains("has no video"), "{err:#}");
}

#[test]
fn failures_name_the_file() {
    let missing = fixtures().join("nothing-here.mp4");
    let err = media_info(&missing).unwrap_err();
    assert!(format!("{err:#}").contains("does not exist"), "{err:#}");

    let garbage = fixture("garbage.mp4");
    let err = media_info(&garbage).unwrap_err();
    assert!(format!("{err:#}").contains("garbage.mp4"), "{err:#}");
    assert!(open_media(&garbage, MediaPart::Video).is_err());

    let video_only = fixture("video-only.mp4");
    let info = media_info(&video_only).unwrap();
    assert_eq!((info.video, info.audio), (Some((160, 120)), false));
    let err = open_media(&video_only, MediaPart::Audio).err().unwrap();
    assert!(format!("{err:#}").contains("has no audio"), "{err:#}");
}

#[test]
fn a_reader_dropped_part_way_stops_cleanly() {
    let path = fixture("h264-aac.mp4");
    for _ in 0..5 {
        let mut reader = open_media(&path, MediaPart::Video).unwrap();
        assert!(reader.next_video().unwrap().is_some());
        drop(reader);
    }
}

#[test]
fn missing_codec_names_the_codec_and_package() {
    let missing = Missing {
        description: "H.264 (High Profile) decoder".into(),
        caps: "video/x-h264, profile=(string)high".into(),
    };
    let text = missing.explain_with("/tmp/clip.mp4", false);
    assert!(text.contains("H.264 (High Profile) decoder"), "{text}");
    assert!(text.contains("video/x-h264"), "{text}");
    assert!(text.contains("gstreamer1.0-libav"), "{text}");
    // With the decoder there, the missing piece is the stream's parser.
    let text = missing.explain_with("/tmp/clip.h264", true);
    assert!(
        text.contains("no parser") && text.contains("gstreamer1.0-plugins-bad"),
        "{text}"
    );
    assert_eq!(package_for("video/x-vp9").0, "gstreamer1.0-plugins-good");
    assert_eq!(package_for("audio/x-opus").0, "gstreamer1.0-plugins-base");
    assert_eq!(package_for("video/x-av1").0, "gstreamer1.0-plugins-bad");
}

#[test]
fn doctor_lists_decoders() {
    let check = decoders_check();
    assert!(check.detail.contains("H.264:"), "{}", check.detail);
    assert!(check.detail.contains("VP9:"), "{}", check.detail);
    assert!(check.detail.contains("Opus:"), "{}", check.detail);
}

/// Run alone with the H.264 decoders ranked out, as on a system without
/// them: `GST_PLUGIN_FEATURE_RANK=avdec_h264:NONE,openh264dec:NONE,
/// v4l2h264dec:NONE,vah264dec:NONE cargo test … -- --ignored missing_h264`.
#[test]
#[ignore = "needs GST_PLUGIN_FEATURE_RANK hiding the H.264 decoders"]
fn missing_h264_decoder_names_codec_and_package() {
    let path = fixture("h264-aac.mp4");
    for err in [
        media_info(&path).unwrap_err(),
        open_media(&path, MediaPart::Audio).err().unwrap(),
    ] {
        let text = format!("{err:#}");
        assert!(text.contains("h264-aac.mp4"), "{text}");
        assert!(text.contains("H.264"), "{text}");
        assert!(text.contains("video/x-h264"), "{text}");
        assert!(text.contains("gstreamer1.0-libav"), "{text}");
    }
    // The WAV needs no H.264 decoder and still plays.
    assert!(media_info(&fixture("mono.wav")).is_ok());
}

#[test]
fn a_real_jump_in_sound_keeps_its_time() {
    let (samples, chunks) = sound(&fixture("gap.webm"));
    let mut jumps = Vec::new();
    let mut end = chunks[0].0;
    for (at, frames) in &chunks {
        let gap = at.as_secs_f64() - end.as_secs_f64();
        if gap.abs() > 0.0005 {
            jumps.push((*at, gap));
        }
        end = *at + Duration::from_secs_f64(*frames as f64 / f64::from(SAMPLE_RATE));
    }
    // Exactly one jump, of the file's 30 ms, half way through; every other
    // chunk continues the last exactly.
    assert_eq!(jumps.len(), 1, "jumps {jumps:?}");
    let (at, gap) = jumps[0];
    assert!((gap - 0.03).abs() < 0.002, "jump of {gap} s at {at:?}");
    assert!(
        close(at, Duration::from_millis(530), Duration::from_millis(40)),
        "jump at {at:?}"
    );
    // The sound after it stays on the file's timeline: it ends 30 ms late.
    assert!(
        close(end, Duration::from_millis(1030), Duration::from_millis(25)),
        "ends at {end:?}"
    );
    assert!(samples.len() / 2 > 46_000);
}

/// Whether GStreamer would autoplug `parser` here (the bad plugins).
fn has_parser(parser: &str) -> bool {
    init().unwrap();
    gst::Registry::get()
        .lookup_feature(parser)
        .is_some_and(|feature| feature.rank() > gst::Rank::NONE)
}

#[test]
fn raw_motion_jpeg_is_refused_with_or_without_a_parser() {
    // A JPEG stream carries no frame rate, so nothing can estimate its
    // length. jpegparse (gstreamer1.0-plugins-bad) is autoplugged from
    // GStreamer 1.26 (rank none before) and reports framerate 0/1: the
    // file is refused either way, never played at length zero.
    let text = format!("{:#}", media_info(&fixture("raw.mjpeg")).unwrap_err());
    assert!(
        text.contains("raw.mjpeg") && text.contains("duration unknown"),
        "raw.mjpeg (jpegparse autoplugged: {}): {text}",
        has_parser("jpegparse")
    );
}

#[test]
fn raw_h264_plays_with_a_parser_or_is_refused() {
    // No container: only h264parse (gstreamer1.0-plugins-bad) estimates a
    // length, from the stream's frame rate. With it the stream plays at
    // that length; without it the file is refused, never played at length
    // zero.
    let result = media_info(&fixture("raw.h264"));
    if has_parser("h264parse") {
        let info = result.unwrap_or_else(|err| panic!("raw.h264 with h264parse: {err:#}"));
        assert_eq!(info.video, Some((160, 120)));
        assert!(
            close(
                info.duration,
                Duration::from_secs(1),
                Duration::from_millis(60)
            ),
            "raw.h264: {:?}",
            info.duration
        );
    } else {
        let text = format!("{:#}", result.unwrap_err());
        assert!(
            text.contains("raw.h264")
                && (text.contains("duration unknown") || text.contains("no parser")),
            "raw.h264 without h264parse: {text}"
        );
    }
}

#[test]
fn streams_whose_parser_estimates_a_length_play() {
    // ADTS AAC and MP3 without a Xing header: parsers estimate the length
    // from the bitrate once data flows.
    for name in ["raw.aac", "plain.mp3"] {
        let info = media_info(&fixture(name)).unwrap();
        assert!(info.audio && info.video.is_none());
        assert!(
            close(
                info.duration,
                Duration::from_secs(1),
                Duration::from_millis(60)
            ),
            "{name}: {:?}",
            info.duration
        );
    }
}

#[test]
fn live_recorded_webm_has_its_real_length() {
    // Picture and sound: the demuxer first says about 2 s of 3.
    let path = fixture("live.webm");
    let info = media_info(&path).unwrap();
    assert_eq!(info.video, Some((256, 144)));
    assert!(info.audio);
    assert!(
        close(
            info.duration,
            Duration::from_secs(3),
            Duration::from_millis(60)
        ),
        "{:?}",
        info.duration
    );
    let pictures = pictures(&path);
    assert_pictures(&pictures, (256, 144), 30, 90);
    assert!(pictures.last().unwrap().2 < info.duration);
    let (samples, chunks) = sound(&path);
    assert_contiguous(&chunks);
    assert!((samples.len() / 2).abs_diff(144_000) < 1_000);

    // Sound only: the demuxer says 121 ms of 1 s and never that it changed.
    let info = media_info(&fixture("live-sound.webm")).unwrap();
    assert!(
        close(
            info.duration,
            Duration::from_secs(1),
            Duration::from_millis(60)
        ),
        "{:?}",
        info.duration
    );
}

#[test]
fn matroska_header_says_whether_its_length_is_stored() {
    assert!(!matroska_lacks_duration(&fixture("vp9-opus.webm")));
    assert!(matroska_lacks_duration(&fixture("live.webm")));
    assert!(matroska_lacks_duration(&fixture("live-sound.webm")));
    assert!(!matroska_lacks_duration(&fixture("h264-aac.mp4")));
    // Truncated or hostile headers never panic.
    let header = [0x1A, 0x45, 0xDF, 0xA3, 0x80];
    let segment = [
        0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    ];
    let with = |rest: &[u8]| [&header[..], &segment, rest].concat();
    // A SeekHead naming Info's ID as data, then Info with a Duration.
    let seek = [0x11, 0x4D, 0x9B, 0x74, 0x84, 0x15, 0x49, 0xA9, 0x66];
    let info = [0x15, 0x49, 0xA9, 0x66, 0x84, 0x44, 0x89, 0x81, 0x00];
    assert_eq!(
        ebml_info_lacks_duration(&with(&[&seek[..], &info].concat())),
        Some(false)
    );
    assert_eq!(
        ebml_info_lacks_duration(&with(&[0x15, 0x49, 0xA9, 0x66, 0x80])),
        Some(true)
    );
    assert_eq!(ebml_info_lacks_duration(&with(&seek)), None);
    assert_eq!(
        ebml_info_lacks_duration(&with(&[0x15, 0x49, 0xA9, 0x66, 0xFF, 0x00])),
        None
    );
    assert_eq!(ebml_info_lacks_duration(&[0x1A, 0x45, 0xDF]), None);
    assert_eq!(ebml_info_lacks_duration(&[0; 64]), None);
}

#[test]
fn a_live_recording_too_long_to_read_through_is_refused() {
    // No time to read it through: the length read so far is never used.
    let path = fixture("live.webm");
    let so_far = Some(Duration::from_millis(121));
    let err = scan_length(&path, so_far, true, Duration::ZERO).unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("live.webm") && text.contains("-c copy"),
        "{text}"
    );
    // A length that is only an estimate is kept instead.
    assert_eq!(
        scan_length(&path, so_far, false, Duration::ZERO).unwrap(),
        so_far
    );
    // With time, the whole file is read.
    let read = scan_length(&path, so_far, true, SCAN).unwrap().unwrap();
    assert!(
        close(read, Duration::from_secs(3), Duration::from_millis(60)),
        "{read:?}"
    );
}
