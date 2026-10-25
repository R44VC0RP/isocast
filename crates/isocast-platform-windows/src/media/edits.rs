//! The start of a QuickTime/MP4 file's first video and first audio track:
//! their edit lists, which Media Foundation's MPEG-4 source applies on some
//! Windows builds and ignores on others (Windows 10 and Server 2022 label
//! pictures and sound from where the media starts, priming included).
//!
//! A small, bounded reader of untrusted bytes: it walks boxes within their
//! parents, reads at most `MAX_MOOV` bytes, and gives up (`None`) on
//! anything malformed rather than guessing.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// A larger movie header than this isn't read (a two-hour file's is a few MB).
const MAX_MOOV: u64 = 64 << 20;

/// Where a track's media starts showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Edit {
    /// Nothing shows for this long first (empty edits).
    pub delay: Duration,
    /// The media before this time isn't shown (an encoder's priming
    /// samples, a B-frame decoder's reorder delay).
    pub skip: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Edits {
    pub video: Edit,
    pub audio: Edit,
}

/// The edits of `path` if it is a QuickTime/MP4 file with a movie header,
/// else `None`. A track without an edit list starts at its media's start.
pub(crate) fn read(path: &Path) -> Option<Edits> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut at = 0u64;
    let mut first = true;
    while at + 8 <= length {
        file.seek(SeekFrom::Start(at)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        if first
            && !matches!(
                &kind,
                b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot"
            )
        {
            return None;
        }
        first = false;
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
            let mut moov = vec![0; body as usize];
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

fn parse_moov(moov: &[u8]) -> Option<Edits> {
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
