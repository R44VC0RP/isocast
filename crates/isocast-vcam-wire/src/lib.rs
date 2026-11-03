//! One complete little-endian header followed by tightly packed, limited-range
//! Rec.709 NV12. Even repeats carry the full picture so late readers can join.
use std::{
    io,
    path::{Path, PathBuf},
};

pub const HEADER_BYTES: usize = 48;
pub fn destination(socket: &Path, session: u32) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(format!(".vcam-{session}"));
    name.into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    width: u32,
    height: u32,
    fps: u32,
}

impl Mode {
    pub fn new(width: u32, height: u32, fps: u32) -> io::Result<Self> {
        if width < 2
            || height < 2
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || u64::from(width) * u64::from(height) > 3840 * 2160
            || !(1..=240).contains(&fps)
        {
            return Err(invalid(
                "virtual_camera.width/height/fps: expected even NV12 dimensions up to 3840×2160 pixels and 1–240 fps",
            ));
        }
        Ok(Self { width, height, fps })
    }
    pub fn width(self) -> u32 {
        self.width
    }
    pub fn height(self) -> u32 {
        self.height
    }
    pub fn fps(self) -> u32 {
        self.fps
    }
    pub fn nv12_len(self) -> usize {
        self.width as usize * self.height as usize * 3 / 2
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub mode: Mode,
    pub repeated: bool,
    pub sequence: u64,
    pub source_ns: i64,
    pub delivery_ns: i64,
}

impl Header {
    pub fn encode(self) -> [u8; HEADER_BYTES] {
        let mut b = [0; HEADER_BYTES];
        b[..8].copy_from_slice(b"ICVCNV01");
        for (dst, field) in b[8..24].as_chunks_mut::<4>().0.iter_mut().zip([
            self.mode.width,
            self.mode.height,
            self.mode.fps,
            u32::from(self.repeated),
        ]) {
            dst.copy_from_slice(&field.to_le_bytes());
        }
        b[24..32].copy_from_slice(&self.sequence.to_le_bytes());
        b[32..40].copy_from_slice(&self.source_ns.to_le_bytes());
        b[40..48].copy_from_slice(&self.delivery_ns.to_le_bytes());
        b
    }

    /// Sequence and timing are scoped to one pipe connection, not the graph clock.
    pub fn decode(b: &[u8; HEADER_BYTES], previous: Option<Self>) -> io::Result<Self> {
        if &b[..8] != b"ICVCNV01" {
            return Err(invalid("unknown camera protocol"));
        }
        let field = |i| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let mode = Mode::new(field(8), field(12), field(16))?;
        let repeated = match field(20) {
            0 => false,
            1 => true,
            _ => return Err(invalid("invalid camera repeat flag")),
        };
        let h = Self {
            mode,
            repeated,
            sequence: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            source_ns: i64::from_le_bytes(b[32..40].try_into().unwrap()),
            delivery_ns: i64::from_le_bytes(b[40..48].try_into().unwrap()),
        };
        if h.sequence == 0
            || h.source_ns < 0
            || h.source_ns > h.delivery_ns
            || previous.is_some_and(|p| {
                p.mode != mode
                    || p.sequence >= h.sequence
                    || p.delivery_ns >= h.delivery_ns
                    || p.source_ns > h.source_ns
                    || (h.repeated && h.sequence - p.sequence == 1 && h.source_ns != p.source_ns)
            })
        {
            return Err(invalid(
                "camera format, sequence or timestamps changed out of order",
            ));
        }
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_late_join_can_start_with_a_whole_held_picture() {
        let h = Header {
            mode: Mode::new(1920, 1080, 30).unwrap(),
            repeated: true,
            sequence: 42,
            source_ns: 7,
            delivery_ns: 19,
        };
        assert_eq!(Header::decode(&h.encode(), None).unwrap(), h);
        assert_eq!(h.mode.nv12_len(), 3_110_400);
        assert!(Header::decode(&h.encode(), Some(h)).is_err());
        let mut b = h.encode();
        b[20] = 2;
        assert!(Header::decode(&b, None).is_err());
        assert!(Mode::new(u32::MAX - 1, u32::MAX - 1, 30).is_err());
    }
}
