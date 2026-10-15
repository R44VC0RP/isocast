//! Writing a replay: H.264 and AAC samples, already encoded, into one
//! MP4 or QuickTime file. Every sample is in memory, so the file is laid
//! out once and in order: `ftyp`, `moov` (so it plays at once, from
//! anywhere), then `mdat` with the samples in time order, one per chunk.
//!
//! The video track starts at its first sample, a keyframe. The audio
//! track holds whole AAC frames from one before the video's start (a
//! frame needs the one before it to decode cleanly); its edit list skips
//! to the video's start, so both tracks begin together.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use isocast_protocol::Container;

use crate::{SAMPLE_RATE, Time};

/// Samples per channel in an AAC frame.
const AAC_FRAME: u32 = 1024;

/// What a replay file holds.
pub(crate) struct Clip {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The AVC decoder configuration record (the `avcC` box's body).
    pub avc_config: Arc<Vec<u8>>,
    /// Video samples in decode order (which is presentation order: no
    /// B-frames): time, keyframe, NAL units with 4-byte lengths.
    pub video: Vec<(Time, bool, Arc<[u8]>)>,
    /// When the last picture ends.
    pub end: Time,
    /// AAC frames in order, each with the time of its first sample;
    /// consecutive frames are 1024 samples apart.
    pub audio: Vec<(Time, Arc<[u8]>)>,
    /// AAC's AudioSpecificConfig.
    pub audio_config: [u8; 2],
    pub channels: u32,
    pub audio_kbps: u32,
}

/// The video timescale: 60000 when the rate divides it (exact for 24, 25,
/// 30, 50, 60…), else 1000 per frame, as recordings use.
fn video_timescale(fps: u32) -> u32 {
    let fps = fps.max(1);
    if 60_000 % fps == 0 {
        60_000
    } else {
        fps * 1000
    }
}

/// `nanos` in units of `scale`, rounded.
fn units(nanos: i64, scale: u32) -> i64 {
    let n = i128::from(nanos) * i128::from(scale);
    let d = 1_000_000_000i128;
    (if n >= 0 {
        (n + d / 2) / d
    } else {
        (n - d / 2) / d
    }) as i64
}

/// The file's size in bytes, before writing it.
pub(crate) fn size(clip: &Clip) -> u64 {
    let media: u64 = clip.video.iter().map(|s| s.2.len() as u64).sum::<u64>()
        + clip.audio.iter().map(|s| s.1.len() as u64).sum::<u64>();
    // Tables: at most ~16 bytes per sample, plus fixed boxes.
    media + 16 * (clip.video.len() + clip.audio.len()) as u64 + 4096
}

/// Write `clip` to `path`, which must not exist. Returns its size.
pub(crate) fn write(path: &Path, clip: &Clip, container: Container) -> std::io::Result<u64> {
    let mov = container == Container::Mov;
    let Some(&(start, _, _)) = clip.video.first() else {
        return Err(std::io::Error::other("a replay needs at least one picture"));
    };
    let media: u64 = clip.video.iter().map(|s| s.2.len() as u64).sum::<u64>()
        + clip.audio.iter().map(|s| s.1.len() as u64).sum::<u64>();
    let wide = media + (64 << 20) > u64::from(u32::MAX);

    // Samples in time order: (time, is video, index).
    let mut order: Vec<(i64, bool, usize)> =
        Vec::with_capacity(clip.video.len() + clip.audio.len());
    order.extend(
        clip.video
            .iter()
            .enumerate()
            .map(|(i, s)| (s.0.as_nanos(), true, i)),
    );
    order.extend(
        clip.audio
            .iter()
            .enumerate()
            .map(|(i, s)| (s.0.as_nanos(), false, i)),
    );
    order.sort_by_key(|&(t, video, i)| (t, !video, i));

    let ftyp = ftyp(mov);
    // Offsets are filled in once the moov's size is known; it doesn't
    // depend on their values.
    let build = |video_offsets: &[u64], audio_offsets: &[u64]| {
        moov(clip, start, mov, wide, video_offsets, audio_offsets)
    };
    let placeholder_moov = build(&vec![0; clip.video.len()], &vec![0; clip.audio.len()]);
    let mdat_header = if wide { 16 } else { 8 };
    let mut offset = (ftyp.len() + placeholder_moov.len() + mdat_header) as u64;
    let mut video_offsets = vec![0; clip.video.len()];
    let mut audio_offsets = vec![0; clip.audio.len()];
    for &(_, video, i) in &order {
        if video {
            video_offsets[i] = offset;
            offset += clip.video[i].2.len() as u64;
        } else {
            audio_offsets[i] = offset;
            offset += clip.audio[i].1.len() as u64;
        }
    }
    let moov = build(&video_offsets, &audio_offsets);
    debug_assert_eq!(moov.len(), placeholder_moov.len());

    let file = File::create_new(path)?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    out.write_all(&ftyp)?;
    out.write_all(&moov)?;
    if wide {
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&(media + 16).to_be_bytes())?;
    } else {
        out.write_all(&((media + 8) as u32).to_be_bytes())?;
        out.write_all(b"mdat")?;
    }
    for &(_, video, i) in &order {
        if video {
            out.write_all(&clip.video[i].2)?;
        } else {
            out.write_all(&clip.audio[i].1)?;
        }
    }
    let file = out.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    Ok(offset)
}

/// A box of `kind` around `body`.
fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

/// A full box (version 0) with `flags`.
fn full(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = flags.to_be_bytes().to_vec();
    b[0] = 0;
    b.extend_from_slice(body);
    boxed(kind, &b)
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

trait Put {
    fn u8(&mut self, v: u8) -> &mut Self;
    fn u16(&mut self, v: u16) -> &mut Self;
    fn u32(&mut self, v: u32) -> &mut Self;
    fn u64(&mut self, v: u64) -> &mut Self;
    fn zeros(&mut self, n: usize) -> &mut Self;
}

impl Put for Vec<u8> {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn zeros(&mut self, n: usize) -> &mut Self {
        self.resize(self.len() + n, 0);
        self
    }
}

/// The identity matrix of `mvhd` and `tkhd`.
fn matrix(b: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        b.u32(v);
    }
}

fn ftyp(mov: bool) -> Vec<u8> {
    let mut b = Vec::new();
    if mov {
        b.extend_from_slice(b"qt  ");
        b.u32(0x2005_0300);
        b.extend_from_slice(b"qt  ");
    } else {
        b.extend_from_slice(b"isom");
        b.u32(0x200);
        for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
            b.extend_from_slice(brand);
        }
    }
    boxed(b"ftyp", &b)
}

/// A track's edit list: an empty edit of `empty`, then `duration` of its
/// media from `media_time` (movie timescale for durations, the track's for
/// `media_time`).
fn edts(empty: u32, duration: u32, media_time: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.u32(if empty > 0 { 2 } else { 1 });
    if empty > 0 {
        b.u32(empty).u32(u32::MAX).u16(1).u16(0);
    }
    b.u32(duration).u32(media_time).u16(1).u16(0);
    boxed(b"edts", &full(b"elst", 0, &b))
}

fn tkhd(id: u32, duration: u32, audio: bool, width: u32, height: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.u32(0).u32(0).u32(id).u32(0).u32(duration).zeros(8);
    b.u16(0) // layer
        .u16(u16::from(audio)) // alternate group
        .u16(if audio { 0x0100 } else { 0 })
        .u16(0);
    matrix(&mut b);
    b.u32(width << 16).u32(height << 16);
    full(b"tkhd", 0x3, &b)
}

fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.u32(0).u32(0).u32(timescale).u32(duration);
    b.u16(0x55c4).u16(0); // "und"
    full(b"mdhd", 0, &b)
}

fn hdlr(mov: bool, component: &[u8; 4], kind: &[u8; 4], name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    if mov {
        b.extend_from_slice(component);
    } else {
        b.u32(0);
    }
    b.extend_from_slice(kind);
    b.zeros(12);
    if mov {
        // QuickTime: a counted string.
        b.u8(name.len() as u8);
        b.extend_from_slice(name.as_bytes());
    } else {
        b.extend_from_slice(name.as_bytes());
        b.u8(0);
    }
    full(b"hdlr", 0, &b)
}

fn dinf(mov: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.u32(1);
    // Flag 1: the media is in this file.
    b.extend(full(if mov { b"alis" } else { b"url " }, 1, &[]));
    boxed(b"dinf", &full(b"dref", 0, &b))
}

/// Run-length sample durations.
fn stts(durations: impl Iterator<Item = u32>) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for d in durations {
        match runs.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => runs.push((1, d)),
        }
    }
    let mut b = Vec::new();
    b.u32(runs.len() as u32);
    for (count, delta) in runs {
        b.u32(count).u32(delta);
    }
    full(b"stts", 0, &b)
}

/// Sizes, one chunk per sample and the chunks' offsets.
fn sample_tables(
    sizes: impl ExactSizeIterator<Item = usize>,
    offsets: &[u64],
    wide: bool,
) -> Vec<u8> {
    let mut stsc = Vec::new();
    stsc.u32(1).u32(1).u32(1).u32(1);
    let mut stsz = Vec::new();
    stsz.u32(0).u32(sizes.len() as u32);
    for size in sizes {
        stsz.u32(size as u32);
    }
    let mut stco = Vec::new();
    stco.u32(offsets.len() as u32);
    for &offset in offsets {
        if wide {
            stco.u64(offset);
        } else {
            stco.u32(offset as u32);
        }
    }
    concat(&[
        full(b"stsc", 0, &stsc),
        full(b"stsz", 0, &stsz),
        full(if wide { b"co64" } else { b"stco" }, 0, &stco),
    ])
}

fn avc1(clip: &Clip, mov: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.zeros(6).u16(1); // data reference 1
    b.u16(0).u16(0).zeros(12);
    b.u16(clip.width as u16).u16(clip.height as u16);
    b.u32(0x0048_0000).u32(0x0048_0000).u32(0).u16(1);
    let mut name = [0u8; 32];
    let label = b"H.264";
    name[0] = label.len() as u8;
    name[1..=label.len()].copy_from_slice(label);
    b.extend_from_slice(&name);
    b.u16(0x0018).u16(0xffff);
    b.extend(boxed(b"avcC", &clip.avc_config));
    // Rec. 709, limited range, as the encoder tags the pictures.
    let mut colr = Vec::new();
    if mov {
        colr.extend_from_slice(b"nclc");
        colr.u16(1).u16(1).u16(1);
    } else {
        colr.extend_from_slice(b"nclx");
        colr.u16(1).u16(1).u16(1).u8(0);
    }
    b.extend(boxed(b"colr", &colr));
    boxed(b"avc1", &b)
}

/// The descriptor `tag` around `body` (lengths under 128).
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![tag, body.len() as u8];
    b.extend_from_slice(body);
    b
}

fn mp4a(clip: &Clip, mov: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.zeros(6).u16(1);
    // QuickTime's version 1 sound description, which AVFoundation needs
    // for AAC in a .mov (it can't open version 0 with an `esds`).
    b.u16(u16::from(mov)).u16(0).u32(0); // version, revision, vendor
    b.u16(clip.channels as u16).u16(16);
    b.u16(if mov { 0xfffe } else { 0 }).u16(0); // compression id, packet size
    b.u32(SAMPLE_RATE << 16);
    if mov {
        // Samples per packet, bytes per packet, per frame, per sample.
        b.u32(AAC_FRAME).u32(0).u32(0).u32(2);
    }
    let bitrate = clip.audio_kbps * 1000;
    let mut config = Vec::new();
    config.u8(0x40).u8(0x15); // MPEG-4 audio; an audio stream
    config.u8(0).u16(0x1800); // 6144-byte buffer (24 bits)
    config.u32(bitrate).u32(bitrate);
    config.extend(descriptor(0x05, &clip.audio_config));
    let mut es = Vec::new();
    es.u16(0).u8(0);
    es.extend(descriptor(0x04, &config));
    es.extend(descriptor(0x06, &[0x02]));
    let esds = full(b"esds", 0, &descriptor(0x03, &es));
    if mov {
        // QuickTime keeps the `esds` in a `wave` atom: the format, an
        // empty `mp4a`, the `esds`, then a terminator.
        let mut format = Vec::new();
        format.extend_from_slice(b"mp4a");
        b.extend(boxed(
            b"wave",
            &concat(&[
                boxed(b"frma", &format),
                boxed(b"mp4a", &[0; 4]),
                esds,
                vec![0; 8],
            ]),
        ));
    } else {
        b.extend(esds);
    }
    boxed(b"mp4a", &b)
}

/// Every AAC frame needs the one before it decoded first (a roll of -1):
/// without this, AVFoundation leaves out the edit's first samples.
fn roll(frames: u32) -> Vec<u8> {
    let mut sgpd = vec![1, 0, 0, 0];
    sgpd.extend_from_slice(b"roll");
    sgpd.u32(2).u32(1).u16(0xffff);
    let mut sbgp = Vec::new();
    sbgp.extend_from_slice(b"roll");
    sbgp.u32(1).u32(frames).u32(1);
    concat(&[boxed(b"sgpd", &sgpd), full(b"sbgp", 0, &sbgp)])
}

fn moov(
    clip: &Clip,
    start: Time,
    mov: bool,
    wide: bool,
    video_offsets: &[u64],
    audio_offsets: &[u64],
) -> Vec<u8> {
    let scale = video_timescale(clip.fps);
    let since = |t: Time| t.as_nanos() - start.as_nanos();
    // Video: sample times from the start, in the track's units.
    let times: Vec<i64> = clip
        .video
        .iter()
        .map(|s| units(since(s.0), scale))
        .collect();
    let total = units(since(clip.end), scale).max(times.last().copied().unwrap_or(0) + 1);
    let durations = times
        .iter()
        .zip(times.iter().skip(1).chain(std::iter::once(&total)))
        .map(|(a, b)| (b - a).max(1) as u32);
    let video_duration = total as u32;
    let mut stss = Vec::new();
    let keys: Vec<u32> = clip
        .video
        .iter()
        .enumerate()
        .filter(|(_, s)| s.1)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    stss.u32(keys.len() as u32);
    for k in keys {
        stss.u32(k);
    }
    let video_stbl = boxed(
        b"stbl",
        &concat(&[
            full(
                b"stsd",
                0,
                &concat(&[1u32.to_be_bytes().to_vec(), avc1(clip, mov)]),
            ),
            stts(durations),
            full(b"stss", 0, &stss),
            sample_tables(clip.video.iter().map(|s| s.2.len()), video_offsets, wide),
        ]),
    );
    let mut vmhd = Vec::new();
    vmhd.u16(0).u16(0).u16(0).u16(0);
    let video_minf = boxed(
        b"minf",
        &concat(&[
            full(b"vmhd", 1, &vmhd),
            if mov {
                hdlr(true, b"dhlr", b"alis", "DataHandler")
            } else {
                Vec::new()
            },
            dinf(mov),
            video_stbl,
        ]),
    );
    let video_trak = boxed(
        b"trak",
        &concat(&[
            tkhd(1, video_duration, false, clip.width, clip.height),
            edts(0, video_duration, 0),
            boxed(
                b"mdia",
                &concat(&[
                    mdhd(scale, video_duration),
                    hdlr(mov, b"mhlr", b"vide", "VideoHandler"),
                    video_minf,
                ]),
            ),
        ]),
    );

    let mut traks = vec![video_trak];
    if let Some(&(first, _)) = clip.audio.first() {
        let frames = clip.audio.len() as u32;
        let media_duration = frames * AAC_FRAME;
        // Where the media starts against the video, in samples: positive
        // skips the media's head (its priming frame), negative waits.
        let skip = units(start.as_nanos() - first.as_nanos(), SAMPLE_RATE);
        let (empty, media_time) = if skip >= 0 {
            (0, skip as u32)
        } else {
            (units(since(first), scale) as u32, 0)
        };
        // The edit can't run past the audio there is.
        let left = i64::from(media_duration.saturating_sub(media_time));
        let available = units(left * 1_000_000_000 / i64::from(SAMPLE_RATE), scale) as u32;
        let segment = video_duration.saturating_sub(empty).min(available);
        let mut smhd = Vec::new();
        smhd.u16(0).u16(0);
        let audio_stbl = boxed(
            b"stbl",
            &concat(&[
                full(
                    b"stsd",
                    0,
                    &concat(&[1u32.to_be_bytes().to_vec(), mp4a(clip, mov)]),
                ),
                stts(std::iter::repeat_n(AAC_FRAME, frames as usize)),
                sample_tables(clip.audio.iter().map(|s| s.1.len()), audio_offsets, wide),
                roll(frames),
            ]),
        );
        let audio_minf = boxed(
            b"minf",
            &concat(&[
                full(b"smhd", 0, &smhd),
                if mov {
                    hdlr(true, b"dhlr", b"alis", "DataHandler")
                } else {
                    Vec::new()
                },
                dinf(mov),
                audio_stbl,
            ]),
        );
        traks.push(boxed(
            b"trak",
            &concat(&[
                tkhd(2, empty + segment, true, 0, 0),
                edts(empty, segment, media_time),
                boxed(
                    b"mdia",
                    &concat(&[
                        mdhd(SAMPLE_RATE, media_duration),
                        hdlr(mov, b"mhlr", b"soun", "SoundHandler"),
                        audio_minf,
                    ]),
                ),
            ]),
        ));
    }
    let mut mvhd = Vec::new();
    mvhd.u32(0).u32(0).u32(scale).u32(video_duration);
    mvhd.u32(0x0001_0000).u16(0x0100).zeros(10);
    matrix(&mut mvhd);
    mvhd.zeros(24).u32(traks.len() as u32 + 1);
    let mut parts = vec![full(b"mvhd", 0, &mvhd)];
    parts.extend(traks);
    boxed(b"moov", &concat(&parts))
}
