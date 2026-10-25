//! The start of a QuickTime/MP4 file's first video and first audio track:
//! their edit lists. Windows' Media Foundation applies them on some builds
//! and ignores them on others (Windows 10 and Server 2022 label pictures
//! and sound from where the media starts, priming included), so its media
//! reader reads them itself.
//!
//! A small, bounded reader of untrusted bytes (fuzzed: `fuzz/` mp4_edits):
//! it walks at most `MAX_TOP_BOXES` top-level boxes, boxes only within
//! their parents, reads at most `MAX_MOOV` bytes, never recurses, and gives
//! up (`None`) on anything malformed rather than guessing.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// A larger movie header than this isn't read (a two-hour file's is a few MB).
pub const MAX_MOOV: u64 = 64 << 20;

/// Top-level boxes looked at before the movie header (a real file has a
/// handful), so a file of tiny boxes can't keep the reader seeking.
pub const MAX_TOP_BOXES: usize = 1024;

/// Where a track's media starts showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Edit {
    /// Nothing shows for this long first (empty edits).
    pub delay: Duration,
    /// The media before this time isn't shown (an encoder's priming
    /// samples, a B-frame decoder's reorder delay).
    pub skip: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Edits {
    pub video: Edit,
    pub audio: Edit,
}

/// The edits of `path` if it is a QuickTime/MP4 file with a movie header,
/// else `None`. A track without an edit list starts at its media's start.
pub fn read(path: &Path) -> Option<Edits> {
    read_from(&mut File::open(path).ok()?)
}

/// The same, from any seekable bytes.
pub fn read_from<R: Read + Seek>(file: &mut R) -> Option<Edits> {
    let length = file.seek(SeekFrom::End(0)).ok()?;
    let mut at = 0u64;
    for index in 0..MAX_TOP_BOXES {
        if at.checked_add(8)? > length {
            return None;
        }
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        if index == 0
            && !matches!(
                &kind,
                b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot"
            )
        {
            return None;
        }
        let (size, header_len) = match u32::from_be_bytes(header[..4].try_into().ok()?) {
            0 => (length - at, 8),
            1 => {
                file.read_exact(&mut header[8..16]).ok()?;
                (u64::from_be_bytes(header[8..16].try_into().ok()?), 16)
            }
            size => (u64::from(size), 8),
        };
        if size < header_len || at.checked_add(size)? > length {
            return None;
        }
        if &kind == b"moov" {
            let body = size - header_len;
            if body > MAX_MOOV {
                return None;
            }
            let mut moov = vec![0; usize::try_from(body).ok()?];
            file.read_exact(&mut moov).ok()?;
            return parse_moov(&moov);
        }
        at += size;
    }
    None
}

/// The children of a box body: (type, body).
fn boxes(mut data: &[u8]) -> impl Iterator<Item = Option<(&[u8], &[u8])>> {
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let parsed = (|| {
            let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
            let kind = data.get(4..8)?;
            let (size, header) = match size {
                0 => (data.len(), 8),
                1 => (
                    usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
                    16,
                ),
                size => (size as usize, 8),
            };
            if size < header || size > data.len() {
                return None;
            }
            let body = &data[header..size];
            let rest = &data[size..];
            Some((kind, body, rest))
        })();
        match parsed {
            Some((kind, body, rest)) => {
                data = rest;
                Some(Some((kind, body)))
            }
            None => {
                data = &[];
                Some(None)
            }
        }
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
    for entry in boxes(data) {
        let (found, body) = entry?;
        if found == kind {
            return Some(body);
        }
    }
    None
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// A full box's timescale: version 0 at byte 12, version 1 at 20.
fn timescale(body: &[u8]) -> Option<u32> {
    let scale = match body.first()? {
        0 => be_u32(body, 12)?,
        1 => be_u32(body, 20)?,
        _ => return None,
    };
    (scale > 0).then_some(scale)
}

fn seconds(value: u64, scale: u32) -> Option<Duration> {
    let nanos = u128::from(value) * 1_000_000_000 / u128::from(scale);
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

/// The edits in a movie header's body (the `moov` box without its header).
pub fn parse_moov(moov: &[u8]) -> Option<Edits> {
    let movie_scale = timescale(child(moov, b"mvhd")?)?;
    let (mut video, mut audio) = (None, None);
    for entry in boxes(moov) {
        let (kind, trak) = entry?;
        if kind != b"trak" {
            continue;
        }
        let mdia = child(trak, b"mdia")?;
        let handler = child(mdia, b"hdlr")?.get(8..12)?;
        let slot = match handler {
            b"vide" if video.is_none() => &mut video,
            b"soun" if audio.is_none() => &mut audio,
            _ => continue,
        };
        let media_scale = timescale(child(mdia, b"mdhd")?)?;
        let edit = match child(trak, b"edts").map(|edts| child(edts, b"elst")) {
            None => Edit::default(),
            Some(elst) => edit(elst?, movie_scale, media_scale)?,
        };
        *slot = Some(edit);
    }
    Some(Edits {
        video: video.unwrap_or_default(),
        audio: audio.unwrap_or_default(),
    })
}

/// Leading empty edits (media time -1) delay the track; the first real
/// edit's media time is where its media starts showing.
fn edit(elst: &[u8], movie_scale: u32, media_scale: u32) -> Option<Edit> {
    let version = *elst.first()?;
    let count = be_u32(elst, 4)? as usize;
    let size = match version {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    let mut result = Edit::default();
    for index in 0..count.min(64) {
        let at = 8 + index * size;
        let (duration, media_time) = match version {
            0 => (
                u64::from(be_u32(elst, at)?),
                i64::from(be_u32(elst, at + 4)? as i32),
            ),
            _ => (be_u64(elst, at)?, be_u64(elst, at + 8)? as i64),
        };
        if media_time == -1 {
            result.delay = result.delay.checked_add(seconds(duration, movie_scale)?)?;
            continue;
        }
        result.skip = seconds(u64::try_from(media_time).ok()?, media_scale)?;
        break;
    }
    Some(result)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A version 0 mvhd/mdhd body with `scale` at byte 12.
    fn timed(scale: u32) -> Vec<u8> {
        let mut body = vec![0; 20];
        body[12..16].copy_from_slice(&scale.to_be_bytes());
        body
    }

    fn trak(handler: &[u8; 4], mdhd: Vec<u8>, elst: Option<Vec<u8>>) -> Vec<u8> {
        let mut hdlr = vec![0; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 12]);
        let mdia = [bx(b"mdhd", &mdhd), bx(b"hdlr", &hdlr)].concat();
        let mut body = bx(b"tkhd", &[0; 84]);
        if let Some(elst) = elst {
            body.extend(bx(b"edts", &bx(b"elst", &elst)));
        }
        body.extend(bx(b"mdia", &mdia));
        bx(b"trak", &body)
    }

    /// A movie: video delayed 0.5 s by an empty edit, starting at media
    /// time 1024/15360 (v0 elst); sound skipping 1024/44100 (v1 mdhd and
    /// elst); then a second video track that must be ignored.
    pub(crate) fn sample() -> Vec<u8> {
        let mut video_elst = vec![0, 0, 0, 0, 0, 0, 0, 2];
        for (duration, media_time) in [(500u32, -1i32), (2000, 1024)] {
            video_elst.extend_from_slice(&duration.to_be_bytes());
            video_elst.extend_from_slice(&media_time.to_be_bytes());
            video_elst.extend_from_slice(&[0, 1, 0, 0]);
        }
        let mut audio_mdhd = vec![1, 0, 0, 0];
        audio_mdhd.extend_from_slice(&[0; 16]);
        audio_mdhd.extend_from_slice(&44_100u32.to_be_bytes());
        audio_mdhd.extend_from_slice(&[0; 8]);
        let mut audio_elst = vec![1, 0, 0, 0, 0, 0, 0, 1];
        audio_elst.extend_from_slice(&2000u64.to_be_bytes());
        audio_elst.extend_from_slice(&1024i64.to_be_bytes());
        audio_elst.extend_from_slice(&[0, 1, 0, 0]);
        let moov = [
            bx(b"mvhd", &timed(1000)),
            trak(b"vide", timed(15_360), Some(video_elst)),
            trak(b"soun", audio_mdhd, Some(audio_elst)),
            trak(b"vide", timed(90_000), None),
        ]
        .concat();
        [
            bx(b"ftyp", b"isom\0\0\0\0"),
            bx(b"mdat", &[7; 64]),
            bx(b"moov", &moov),
        ]
        .concat()
    }

    #[test]
    fn edits_of_each_first_track() {
        let edits = read_from(&mut Cursor::new(sample())).unwrap();
        assert_eq!(edits.video.delay, Duration::from_millis(500));
        assert_eq!(
            edits.video.skip,
            Duration::from_nanos(1024 * 1_000_000_000 / 15_360)
        );
        assert_eq!(edits.audio.delay, Duration::ZERO);
        assert_eq!(
            edits.audio.skip,
            Duration::from_nanos(1024 * 1_000_000_000 / 44_100)
        );
        // Not QuickTime, no movie header, or too many boxes before it.
        assert_eq!(
            read_from(&mut Cursor::new(b"RIFF\0\0\0\0WAVE".to_vec())),
            None
        );
        assert_eq!(read_from(&mut Cursor::new(bx(b"ftyp", b"isom"))), None);
        let mut tiny = bx(b"ftyp", b"isom");
        for _ in 0..MAX_TOP_BOXES {
            tiny.extend(bx(b"free", &[]));
        }
        tiny.extend(&sample()[16..]);
        assert_eq!(read_from(&mut Cursor::new(tiny)), None);
    }

    /// The fuzz target's checks on many truncated, mutated and random
    /// inputs, so they run in every `cargo test` (cargo-fuzz goes further).
    #[test]
    fn malformed_input_never_panics() {
        let seed = sample();
        // ftyp (16 bytes), mdat (72), then the movie header's own header.
        let seeds = [seed.clone(), seed[96..].to_vec()];
        assert!(parse_moov(&seeds[1]).is_some());
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..20_000 {
            let mut bytes = seeds[round % 2].clone();
            if round % 5 == 0 {
                bytes.truncate((next() as usize) % (bytes.len() + 1));
            } else {
                for _ in 0..1 + next() % 6 {
                    let at = (next() as usize) % bytes.len().max(1);
                    match next() % 3 {
                        0 if !bytes.is_empty() => bytes[at] = next() as u8,
                        1 => bytes.truncate(at),
                        _ => bytes.insert(at.min(bytes.len()), next() as u8),
                    }
                }
            }
            check(&bytes);
        }
    }

    /// What the fuzz target asserts for any input: read as a movie header
    /// and as a whole file, nothing panics or runs away.
    pub fn check(bytes: &[u8]) {
        let _ = parse_moov(bytes);
        let _ = read_from(&mut Cursor::new(bytes));
    }
}
