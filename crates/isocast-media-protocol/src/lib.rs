//! Private file-reader wire. The opening argv supplies version, operation and parent PID.
//! INIT means the plugin registry is ready. Then stdin carries a PATH (raw Unix
//! bytes) for file operations and a NEXT per requested sample after REDY.
//! Like the browser helper, every record has a four-byte tag and a u32 LE size.
//! Unlike a live page, media has file timestamps and an explicit END. EOF is an error.

use std::io::{self, Read, Write};
use std::time::Duration;

pub const VERSION: &str = "media-v3";
pub const MAX_TEXT: usize = 64 * 1024;
const MAX_AUDIO: usize = 16 * 1024 * 1024;

/// Audio inside the engine is 48 kHz, 32-bit float, interleaved.
pub const SAMPLE_RATE: u32 = 48_000;

/// Which part of a media file a reader decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPart {
    Video,
    Audio,
}

/// What a media file holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaInfo {
    /// Width and height of its picture, if it has one.
    pub video: Option<(u32, u32)>,
    pub audio: bool,
    pub duration: Duration,
}

/// Decodes one part of a media file, from its start, as fast as asked.
pub trait MediaReader: Send {
    type Frame;

    /// The next frame and its time from the start of the file; `None` at the end.
    fn next_video(&mut self) -> anyhow::Result<Option<(Self::Frame, Duration)>>;

    /// Interleaved stereo at [`SAMPLE_RATE`] and the time of its first sample;
    /// `None` at the end.
    fn next_audio(&mut self) -> anyhow::Result<Option<(Vec<f32>, Duration)>>;
}

#[derive(Debug, PartialEq)]
pub enum Header {
    Path(usize),
    Check(usize), // doctor Check JSON
    Info(usize),  // (video dimensions, has audio, duration in ns), JSON
    Initialized,
    Ready,
    Next,
    End,
    Fail(usize),
    Video {
        ns: u64,
        w: u32,
        h: u32,
        bytes: usize,
    },
    Audio {
        ns: u64,
        bytes: usize,
    }, // packed stereo f32 LE at 48 kHz
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid media record")
}

pub fn read_header(input: &mut impl Read) -> io::Result<Header> {
    let mut h = [0; 8];
    input.read_exact(&mut h)?;
    let n = u32::from_le_bytes(h[4..].try_into().unwrap()) as usize;
    Ok(match (&h[..4], n) {
        (b"PATH", 0..=4096) => Header::Path(n),
        (b"INFO", 1..=MAX_TEXT) => Header::Info(n),
        (b"CHCK", 1..=MAX_TEXT) => Header::Check(n),
        (b"FAIL", 1..=MAX_TEXT) => Header::Fail(n),
        (b"INIT", 0) => Header::Initialized,
        (b"REDY", 0) => Header::Ready,
        (b"NEXT", 0) => Header::Next,
        (b"END!", 0) => Header::End,
        (b"VBGR", 20..) => {
            let mut meta = [0; 16];
            input.read_exact(&mut meta)?;
            let ns = u64::from_le_bytes(meta[..8].try_into().unwrap());
            let w = u32::from_le_bytes(meta[8..12].try_into().unwrap());
            let h = u32::from_le_bytes(meta[12..].try_into().unwrap());
            if !(1..=8192).contains(&w)
                || !(1..=8192).contains(&h)
                || n - 16 != (w * h * 4) as usize
            {
                return Err(invalid());
            }
            Header::Video {
                ns,
                w,
                h,
                bytes: n - 16,
            }
        }
        (b"AF32", 16..) if n - 8 <= MAX_AUDIO && n.is_multiple_of(8) => {
            let mut pts = [0; 8];
            input.read_exact(&mut pts)?;
            Header::Audio {
                ns: u64::from_le_bytes(pts),
                bytes: n - 8,
            }
        }
        _ => return Err(invalid()),
    })
}

/// Scatter without making a second frame-sized byte vector.
pub fn write_record(out: &mut impl Write, tag: &[u8; 4], fields: &[&[u8]]) -> io::Result<()> {
    let n = fields
        .iter()
        .try_fold(0u32, |n, f| n.checked_add(f.len().try_into().ok()?))
        .ok_or_else(invalid)?;
    out.write_all(tag)?;
    out.write_all(&n.to_le_bytes())?;
    for field in fields {
        out.write_all(field)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_and_terminal_records_keep_their_boundaries() {
        let cases: &[(&[u8; 4], &[u8], Header)] = &[
            (b"INIT", b"", Header::Initialized),
            (b"PATH", b"/x\xff", Header::Path(3)),
            (b"INFO", b"[]", Header::Info(2)),
            (b"CHCK", b"{}", Header::Check(2)),
            (b"REDY", b"", Header::Ready),
            (b"NEXT", b"", Header::Next),
            (b"FAIL", b"bad", Header::Fail(3)),
            (b"END!", b"", Header::End),
        ];
        let mut wire = Vec::new();
        for (tag, body, _) in cases {
            write_record(&mut wire, tag, &[body]).unwrap();
        }
        let mut input = wire.as_slice();
        for (_, body, expected) in cases {
            assert_eq!(&read_header(&mut input).unwrap(), expected);
            let (payload, rest) = input.split_at(body.len());
            assert_eq!(payload, *body);
            input = rest;
        }
        assert!(input.is_empty());
        assert_eq!(
            read_header(&mut input).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_unknown_tags_bad_lengths_and_truncated_metadata() {
        for (tag, n) in [
            (b"JUNK", 0),
            (b"INIT", 1),
            (b"END!", 1),
            (b"NEXT", 1),
            (b"REDY", 1),
            (b"PATH", 4097),
            (b"INFO", 0),
            (b"FAIL", MAX_TEXT as u32 + 1),
            (b"CHCK", u32::MAX),
            (b"AF32", 8),
            (b"AF32", 17),
            (b"AF32", MAX_AUDIO as u32 + 16),
        ] {
            let wire = [tag.as_slice(), &n.to_le_bytes()].concat();
            assert_eq!(
                read_header(&mut &wire[..]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for (tag, n, meta) in [(b"AF32", 16u32, 8usize), (b"VBGR", 20, 16)] {
            let wire = [tag.as_slice(), &n.to_le_bytes(), &vec![0; meta]].concat();
            for end in 0..wire.len() {
                assert_eq!(
                    read_header(&mut &wire[..end]).unwrap_err().kind(),
                    io::ErrorKind::UnexpectedEof
                );
            }
        }
        for (w, h) in [(0u32, 1u32), (8193, 1), (1, u32::MAX), (u32::MAX, u32::MAX)] {
            let wire = [
                b"VBGR".as_slice(),
                &20u32.to_le_bytes(),
                &0u64.to_le_bytes(),
                &w.to_le_bytes(),
                &h.to_le_bytes(),
            ]
            .concat();
            assert_eq!(
                read_header(&mut &wire[..]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn seeded_timed_samples_round_trip_and_corrupt_sizes_fail() {
        let mut seed = 0x9e37_79b9_u32;
        let mut next = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for i in 0..512 {
            let ns = (u64::from(next()) << 32) | u64::from(next());
            let (w, h) = (1 + next() % 32, 1 + next() % 32);
            let video = i % 2 == 0;
            let bytes = if video {
                (w * h * 4) as usize
            } else {
                ((1 + w) * 8) as usize
            };
            let payload = vec![next() as u8; bytes];
            let mut wire = Vec::new();
            let expected = if video {
                write_record(
                    &mut wire,
                    b"VBGR",
                    &[
                        &ns.to_le_bytes(),
                        &w.to_le_bytes(),
                        &h.to_le_bytes(),
                        &payload,
                    ],
                )
                .unwrap();
                Header::Video { ns, w, h, bytes }
            } else {
                write_record(&mut wire, b"AF32", &[&ns.to_le_bytes(), &payload]).unwrap();
                Header::Audio { ns, bytes }
            };
            let mut input = wire.as_slice();
            assert_eq!(read_header(&mut input).unwrap(), expected);
            assert_eq!(input, payload);
            wire[4] ^= 1; // mismatched pixel count / unaligned PCM
            assert_eq!(
                read_header(&mut &wire[..]).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
