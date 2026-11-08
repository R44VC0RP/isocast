//! What an `isocast-browser` helper (crates/isocast-browser) and isocastd
//! say to each other, the same on every OS (the Linux helper's protocol,
//! crates/isocast-platform-linux/src/browser.rs, shared so Windows speaks
//! it too).
//!
//! isocastd writes the page as the first line of the helper's stdin
//! ([`page_line`]: never in arguments, which other processes can read),
//! then commands, one per line: `reload`, `unload`, `load`. stdin closing
//! ends the helper.
//!
//! The helper answers with records of a 4-byte tag, a little-endian u32
//! payload length and the payload:
//! - `REDY` (empty): the page is loading, and later a beat while the page
//!   is running but has no new picture (still or loading pages);
//! - `FRAM`: u32 width, u32 height, then width × height × 4 bytes of
//!   straight-alpha BGRA;
//! - `FRMT`: u32 width, u32 height, u32 capture age in microseconds, then
//!   width × height × 4 bytes of straight-alpha BGRA;
//! - `AUDI`: u32 sample rate, then interleaved stereo f32 samples;
//! - `AUDT`: u32 sample rate, u8 kind (0 continuous, 1 resumed), u64 page
//!   `Date.now()` wall time in microseconds, then interleaved stereo f32;
//! - `FAIL`: a UTF-8 reason, before exiting.

use std::io::Read;

use anyhow::{Context, Result, bail};

pub const READY: &[u8; 4] = b"REDY";
pub const FRAME: &[u8; 4] = b"FRAM";
pub const TIMED_FRAME: &[u8; 4] = b"FRMT";
pub const AUDIO: &[u8; 4] = b"AUDI";
pub const TIMED_AUDIO: &[u8; 4] = b"AUDT";
pub const FAIL: &[u8; 4] = b"FAIL";

/// Largest payload accepted: an 8192×8192 picture and its header.
pub const MAX_PAYLOAD: u32 = 8192 * 8192 * 4 + 12;
/// Audio sample rates accepted. The page's script chooses the rate, so it
/// is untrusted: resampling a rate of 1 to 48 kHz would size its output at
/// 48,000 times the input.
pub const AUDIO_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;
/// Largest audio payload: over a second at the highest rate (helpers send
/// at most a second per record).
pub const MAX_AUDIO: u32 = 2 << 20;
/// Largest failure reason.
pub const MAX_FAIL: u32 = 4 << 10;

#[derive(Debug, PartialEq, Eq)]
pub enum Header {
    Ready,
    /// A picture of this size; its pixels follow.
    Frame {
        width: u32,
        height: u32,
    },
    /// A picture captured `age_us` ago; its pixels follow.
    TimedFrame {
        width: u32,
        height: u32,
        age_us: u32,
    },
    /// Interleaved stereo f32 at this rate: `len` bytes follow.
    Audio {
        rate: u32,
        len: usize,
    },
    /// Timed interleaved stereo f32 from `audio.js`: `len` bytes follow.
    TimedAudio {
        rate: u32,
        resumed: bool,
        end_us: u64,
        len: usize,
    },
    /// A reason follows, `len` bytes.
    Fail {
        len: usize,
    },
}

/// Read the next record's header (and, for a frame or audio, its first
/// fields); `None` at the end of the stream.
pub fn read_header(input: &mut impl Read) -> Result<Option<Header>> {
    let mut head = [0u8; 8];
    match input.read_exact(&mut head) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("reading the browser helper"),
    }
    let len = u32::from_le_bytes(head[4..8].try_into().expect("4 bytes"));
    if len > MAX_PAYLOAD {
        bail!("browser helper sent an oversized record ({len} bytes)");
    }
    let mut u32_field = || -> Result<u32> {
        let mut field = [0u8; 4];
        input
            .read_exact(&mut field)
            .context("reading the browser helper")?;
        Ok(u32::from_le_bytes(field))
    };
    let tag: &[u8; 4] = head[..4].try_into().expect("4 bytes");
    let limit = match tag {
        READY => 0,
        AUDIO => 4 + MAX_AUDIO,
        TIMED_AUDIO => 13 + MAX_AUDIO,
        FAIL => MAX_FAIL,
        _ => MAX_PAYLOAD,
    };
    if len > limit {
        bail!(
            "browser helper sent an oversized {} record ({len} bytes, at most {limit})",
            String::from_utf8_lossy(tag)
        );
    }
    Ok(Some(match tag {
        READY => Header::Ready,
        FRAME => {
            let (width, height) = (u32_field()?, u32_field()?);
            if len as u64 != 8 + u64::from(width) * u64::from(height) * 4 {
                bail!(
                    "browser helper sent a picture whose size doesn't match ({width}x{height}, {len} bytes)"
                );
            }
            Header::Frame { width, height }
        }
        TIMED_FRAME => {
            let (width, height, age_us) = (u32_field()?, u32_field()?, u32_field()?);
            if len as u64 != 12 + u64::from(width) * u64::from(height) * 4 {
                bail!(
                    "browser helper sent a picture whose size doesn't match ({width}x{height}, {len} bytes)"
                );
            }
            Header::TimedFrame {
                width,
                height,
                age_us,
            }
        }
        AUDIO => {
            let rate = u32_field()?;
            let len =
                len.checked_sub(4)
                    .context("browser helper sent a short audio record")? as usize;
            if !len.is_multiple_of(8) {
                bail!("browser helper sent audio that isn't whole stereo f32 frames");
            }
            if !AUDIO_RATES.contains(&rate) {
                bail!(
                    "browser helper sent audio at {rate} Hz (accepted: {}..={} Hz)",
                    AUDIO_RATES.start(),
                    AUDIO_RATES.end()
                );
            }
            Header::Audio { rate, len }
        }
        TIMED_AUDIO => {
            let rate = u32_field()?;
            let mut meta = [0u8; 9];
            input
                .read_exact(&mut meta)
                .context("reading the browser helper")?;
            let resumed = match meta[0] {
                0 => false,
                1 => true,
                other => bail!("browser helper sent an unknown audio timing kind {other}"),
            };
            let end_us = u64::from_le_bytes(meta[1..9].try_into().expect("8 bytes"));
            let len = len
                .checked_sub(13)
                .context("browser helper sent a short timed audio record")?
                as usize;
            if !len.is_multiple_of(8) {
                bail!("browser helper sent audio that isn't whole stereo f32 frames");
            }
            if !AUDIO_RATES.contains(&rate) {
                bail!(
                    "browser helper sent audio at {rate} Hz (accepted: {}..={} Hz)",
                    AUDIO_RATES.start(),
                    AUDIO_RATES.end()
                );
            }
            Header::TimedAudio {
                rate,
                resumed,
                end_us,
                len,
            }
        }
        FAIL => Header::Fail { len: len as usize },
        tag => bail!(
            "browser helper sent an unknown record {:?}",
            String::from_utf8_lossy(tag)
        ),
    }))
}

/// Where the page is.
#[derive(Debug, PartialEq, Eq)]
pub enum Location {
    Url(String),
    /// A local file's path, as bytes (Unix) or UTF-8 (Windows).
    File(Vec<u8>),
}

/// Bytes as lowercase hex (one word on the page line, whatever they hold).
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The page line isocastd writes first: `page url|file <hex> <hex css>\n`.
pub fn page_line(location: &Location, css: &str) -> String {
    let (kind, bytes) = match location {
        Location::Url(url) => ("url", url.as_bytes()),
        Location::File(path) => ("file", path.as_slice()),
    };
    format!(
        "page {kind} {} {}\n",
        hex(bytes),
        hex(css.trim().as_bytes())
    )
}

/// The page and its CSS from a page line.
pub fn parse_page_line(line: &str) -> Result<(Location, String), String> {
    let bad = || format!("expected \"page url|file <hex> <hex>\" on stdin, got {line:.40?}");
    let unhex = |word: &str| -> Result<Vec<u8>, String> {
        if !word.len().is_multiple_of(2) {
            return Err(bad());
        }
        (0..word.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(word.get(i..i + 2).ok_or_else(bad)?, 16).map_err(|_| bad()))
            .collect()
    };
    let words: Vec<&str> = line.trim_end_matches(['\n', '\r']).split(' ').collect();
    let ["page", kind, location, css] = words[..] else {
        return Err(bad());
    };
    let location = unhex(location)?;
    let css = String::from_utf8(unhex(css)?).map_err(|_| bad())?;
    let location = match kind {
        "url" => Location::Url(String::from_utf8(location).map_err(|_| bad())?),
        "file" => Location::File(location),
        _ => return Err(bad()),
    };
    Ok((location, css))
}

/// A record's tag and length, with room for its payload.
pub fn record(tag: &[u8; 4], payload_len: usize) -> Vec<u8> {
    let mut record = Vec::with_capacity(8 + payload_len);
    record.extend_from_slice(tag);
    record.extend_from_slice(&(payload_len as u32).to_le_bytes());
    record
}

/// Make `record` (a reused buffer) a `FRAM` record of a `width` × `height`
/// picture and return its pixels, zeroed, to fill with straight-alpha BGRA.
pub fn frame(record: &mut Vec<u8>, width: u32, height: u32) -> &mut [u8] {
    let len = width as usize * height as usize * 4;
    record.clear();
    record.extend_from_slice(FRAME);
    record.extend_from_slice(&((8 + len) as u32).to_le_bytes());
    record.extend_from_slice(&width.to_le_bytes());
    record.extend_from_slice(&height.to_le_bytes());
    record.resize(16 + len, 0);
    &mut record[16..]
}

/// Make `record` (a reused buffer) a `FRMT` record of a `width` × `height`
/// picture and return its pixels, zeroed; set the capture age with
/// [`set_frame_age`] once readback finishes.
pub fn timed_frame(record: &mut Vec<u8>, width: u32, height: u32) -> &mut [u8] {
    let len = width as usize * height as usize * 4;
    record.clear();
    record.extend_from_slice(TIMED_FRAME);
    record.extend_from_slice(&((12 + len) as u32).to_le_bytes());
    record.extend_from_slice(&width.to_le_bytes());
    record.extend_from_slice(&height.to_le_bytes());
    record.extend_from_slice(&0u32.to_le_bytes());
    record.resize(20 + len, 0);
    &mut record[20..]
}

/// Patch the `age_us` field of a `FRMT` record built by [`timed_frame`].
pub fn set_frame_age(record: &mut [u8], age_us: u32) {
    record[16..20].copy_from_slice(&age_us.to_le_bytes());
}

/// An `AUDI` record: the rate and interleaved stereo f32 samples' bytes.
pub fn audio(rate: u32, samples: &[u8]) -> Vec<u8> {
    let mut record = record(AUDIO, 4 + samples.len());
    record.extend_from_slice(&rate.to_le_bytes());
    record.extend_from_slice(samples);
    record
}

/// An `AUDT` record: rate, resumed flag, page `Date.now()` wall time in
/// microseconds (`end_us`), and sample bytes.
pub fn timed_audio(rate: u32, resumed: bool, end_us: u64, samples: &[u8]) -> Vec<u8> {
    let mut record = record(TIMED_AUDIO, 13 + samples.len());
    record.extend_from_slice(&rate.to_le_bytes());
    record.push(u8::from(resumed));
    record.extend_from_slice(&end_us.to_le_bytes());
    record.extend_from_slice(samples);
    record
}

/// A `FAIL` record with `reason` (cut to [`MAX_FAIL`] bytes).
pub fn fail(reason: &str) -> Vec<u8> {
    let mut end = reason.len().min(MAX_FAIL as usize);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut record = record(FAIL, end);
    record.extend_from_slice(&reason.as_bytes()[..end]);
    record
}

/// Premultiplied BGRA (what renderers make) to straight alpha, in place.
pub fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(pixel[3]);
        if a != 0 && a != 255 {
            for c in &mut pixel[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_records_read_back() {
        let mut bytes = record(READY, 0);
        let mut picture = Vec::new();
        frame(&mut picture, 2, 1).copy_from_slice(&[7; 8]);
        bytes.extend(&picture);
        let mut timed_picture = Vec::new();
        timed_frame(&mut timed_picture, 2, 1).copy_from_slice(&[9; 8]);
        set_frame_age(&mut timed_picture, 14_500);
        bytes.extend(&timed_picture);
        bytes.extend(audio(44100, &[0; 8]));
        bytes.extend(timed_audio(48000, true, 75_000, &[1; 8]));
        bytes.extend(fail("no runtime"));
        let mut input = &bytes[..];
        assert_eq!(read_header(&mut input).unwrap(), Some(Header::Ready));
        assert_eq!(
            read_header(&mut input).unwrap(),
            Some(Header::Frame {
                width: 2,
                height: 1
            })
        );
        assert_eq!(&input[..8], &[7; 8]);
        input = &input[8..];
        assert_eq!(
            read_header(&mut input).unwrap(),
            Some(Header::TimedFrame {
                width: 2,
                height: 1,
                age_us: 14_500,
            })
        );
        assert_eq!(&input[..8], &[9; 8]);
        input = &input[8..];
        assert_eq!(
            read_header(&mut input).unwrap(),
            Some(Header::Audio {
                rate: 44100,
                len: 8
            })
        );
        input = &input[8..];
        assert_eq!(
            read_header(&mut input).unwrap(),
            Some(Header::TimedAudio {
                rate: 48000,
                resumed: true,
                end_us: 75_000,
                len: 8,
            })
        );
        assert_eq!(&input[..8], &[1; 8]);
        input = &input[8..];
        assert_eq!(
            read_header(&mut input).unwrap(),
            Some(Header::Fail { len: 10 })
        );
        assert_eq!(input, b"no runtime");
        input = &input[10..];
        assert_eq!(read_header(&mut input).unwrap(), None);
    }

    #[test]
    fn bad_records_are_errors_not_guesses() {
        // A picture whose size doesn't match its length.
        let mut bad = FRAME.to_vec();
        bad.extend(&100u32.to_le_bytes());
        bad.extend(&2u32.to_le_bytes());
        bad.extend(&1u32.to_le_bytes());
        assert!(read_header(&mut &bad[..]).is_err());
        // Oversized, unknown, a payload on an empty record, odd audio, a
        // page-chosen rate out of range.
        let mut huge = FRAME.to_vec();
        huge.extend(&u32::MAX.to_le_bytes());
        assert!(read_header(&mut &huge[..]).is_err());
        assert!(read_header(&mut &b"WHAT\0\0\0\0"[..]).is_err());
        assert!(read_header(&mut &b"REDY\x01\0\0\0x"[..]).is_err());
        let mut odd = audio(48000, &[0; 8]);
        odd[4] = 4 + 6;
        assert!(read_header(&mut &odd[..]).is_err());
        assert!(read_header(&mut &audio(1, &[0; 8])[..]).is_err());
        let mut bad_kind = timed_audio(48000, false, 0, &[0; 8]);
        bad_kind[12] = 2;
        assert!(read_header(&mut &bad_kind[..]).is_err());
    }

    #[test]
    fn page_lines_round_trip_and_reject_garbage() {
        let line = page_line(
            &Location::File("C:\\a b\\p.html".into()),
            " body { color: red } ",
        );
        assert!(line.starts_with("page file ") && line.ends_with('\n'));
        assert!(!line.contains("a b"), "hex, not the path: {line}");
        assert_eq!(
            parse_page_line(&line).unwrap(),
            (
                Location::File("C:\\a b\\p.html".into()),
                "body { color: red }".into()
            )
        );
        let line = page_line(&Location::Url("https://x/?t=1".into()), "");
        assert_eq!(
            parse_page_line(&line).unwrap(),
            (Location::Url("https://x/?t=1".into()), String::new())
        );
        assert!(parse_page_line("page url zz \n").is_err());
        assert!(parse_page_line("reload\n").is_err());
    }

    #[test]
    fn unpremultiplies_partly_transparent_pixels_only() {
        let mut pixels = [64, 32, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut pixels);
        assert_eq!(pixels, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }
}
