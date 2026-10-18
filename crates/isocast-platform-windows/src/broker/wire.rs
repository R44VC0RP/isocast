//! Private, versioned pipe protocol. No paths, executable names, method names,
//! audio or pixel bytes cross this boundary. Handles in Frame are BORROWED
//! child handles: only the parent may duplicate them, while the lease is held.

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use isocast_engine::CaptureFormat;

use super::IsolatedCaptureStats;
use crate::{CaptureStats, CaptureTarget};

const MAGIC: u32 = 0x50414349; // ICAP, little endian
const VERSION: u16 = 1;
const MAX_MESSAGE: usize = 4096;
pub(super) const MAX_LEASES: usize = 32;
pub(super) const MAX_SURFACES: usize = 64;

pub(super) struct Init {
    pub target: CaptureTarget,
    pub fps: u32,
    pub cursor: bool,
    pub luid: (u32, i32),
}

pub(super) struct Picture {
    pub sequence: u64,
    pub surface: u64,
    pub texture: u64,
    pub fence: u64,
    pub ready: u64,
    pub width: u32,
    pub height: u32,
    pub nanos: i64,
}

pub(super) enum Message {
    Init(Init),
    Ready(CaptureFormat, (u32, i32)),
    Frame(Picture),
    Ack(u64),
    Stop,
    Stats(IsolatedCaptureStats),
    Error(String),
}

pub(super) fn geometry(width: u32, height: u32) -> Result<()> {
    if !(1..=16384).contains(&width) || !(1..=16384).contains(&height) {
        bail!("capture helper geometry must be within 1..=16384 pixels per side");
    }
    Ok(())
}

pub(super) fn fps(value: u32) -> Result<()> {
    if !(1..=240).contains(&value) {
        bail!("screen.fps: {value}; valid range: 1..=240");
    }
    Ok(())
}

fn handle(value: u64) -> Result<()> {
    if value == 0 || value >= isize::MAX as u64 {
        bail!("capture helper returned an invalid handle");
    }
    Ok(())
}

impl Message {
    pub(super) fn write(&self, output: &mut impl Write) -> Result<()> {
        let mut data = Vec::new();
        let kind: u16 = match self {
            Self::Init(init) => {
                let (kind, target) = match init.target {
                    CaptureTarget::PrimaryDisplay => (0u32, 0u64),
                    CaptureTarget::Monitor(handle) => (1, handle as u64),
                    CaptureTarget::Window(handle) => (2, handle as u64),
                };
                data.extend(kind.to_le_bytes());
                data.extend(target.to_le_bytes());
                data.extend(init.fps.to_le_bytes());
                data.extend(u32::from(init.cursor).to_le_bytes());
                data.extend(init.luid.0.to_le_bytes());
                data.extend(init.luid.1.to_le_bytes());
                1
            }
            Self::Ready(format, luid) => {
                data.extend(format.width.to_le_bytes());
                data.extend(format.height.to_le_bytes());
                data.extend(format.fps.to_le_bytes());
                data.extend(luid.0.to_le_bytes());
                data.extend(luid.1.to_le_bytes());
                2
            }
            Self::Frame(frame) => {
                for value in [
                    frame.sequence,
                    frame.surface,
                    frame.texture,
                    frame.fence,
                    frame.ready,
                ] {
                    data.extend(value.to_le_bytes());
                }
                data.extend(frame.width.to_le_bytes());
                data.extend(frame.height.to_le_bytes());
                data.extend(1u32.to_le_bytes()); // BGRA8, the only wire format
                data.extend(frame.nanos.to_le_bytes());
                3
            }
            Self::Ack(sequence) => {
                data.extend(sequence.to_le_bytes());
                4
            }
            Self::Stop => 5,
            Self::Stats(stats) => {
                for value in [
                    stats.capture.received,
                    stats.capture.delivered,
                    stats.capture.pool_dropped,
                    stats.capture.rate_skipped,
                    stats.capture.resize_skipped,
                    stats.published,
                    stats.backpressure_dropped,
                ] {
                    data.extend(value.to_le_bytes());
                }
                6
            }
            Self::Error(error) => {
                let mut end = error.len().min(MAX_MESSAGE);
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                data.extend_from_slice(&error.as_bytes()[..end]);
                7
            }
        };
        if data.len() > MAX_MESSAGE {
            bail!("capture helper message exceeds its limit");
        }
        output.write_all(&MAGIC.to_le_bytes())?;
        output.write_all(&VERSION.to_le_bytes())?;
        output.write_all(&kind.to_le_bytes())?;
        output.write_all(&(data.len() as u32).to_le_bytes())?;
        output.write_all(&data)?;
        output.flush()?;
        Ok(())
    }

    pub(super) fn read(input: &mut impl Read) -> Result<Self> {
        let mut header = [0; 12];
        input
            .read_exact(&mut header)
            .context("capture helper pipe closed")?;
        let mut header = Fields(&header);
        if header.u32()? != MAGIC || header.u16()? != VERSION {
            bail!("capture helper protocol/version mismatch");
        }
        let kind = header.u16()?;
        let length = header.u32()? as usize;
        if length > MAX_MESSAGE {
            bail!("capture helper message exceeds {MAX_MESSAGE} bytes");
        }
        let mut data = [0; MAX_MESSAGE];
        input.read_exact(&mut data[..length])?;
        let mut fields = Fields(&data[..length]);
        let message = match kind {
            1 => {
                let kind = fields.u32()?;
                let value = fields.u64()?;
                let target = match kind {
                    0 if value == 0 => CaptureTarget::PrimaryDisplay,
                    1 | 2 => {
                        handle(value)?;
                        if kind == 1 {
                            CaptureTarget::Monitor(value as usize)
                        } else {
                            CaptureTarget::Window(value as usize)
                        }
                    }
                    _ => bail!("capture helper target: expected primary, monitor or window"),
                };
                let rate = fields.u32()?;
                fps(rate)?;
                let cursor = match fields.u32()? {
                    0 => false,
                    1 => true,
                    _ => bail!("capture helper cursor: expected 0 or 1"),
                };
                Self::Init(Init {
                    target,
                    fps: rate,
                    cursor,
                    luid: (fields.u32()?, fields.u32()? as i32),
                })
            }
            2 => {
                let format = CaptureFormat {
                    width: fields.u32()?,
                    height: fields.u32()?,
                    fps: fields.u32()?,
                };
                geometry(format.width, format.height)?;
                fps(format.fps)?;
                Self::Ready(format, (fields.u32()?, fields.u32()? as i32))
            }
            3 => {
                let mut frame = Picture {
                    sequence: fields.u64()?,
                    surface: fields.u64()?,
                    texture: fields.u64()?,
                    fence: fields.u64()?,
                    ready: fields.u64()?,
                    width: fields.u32()?,
                    height: fields.u32()?,
                    nanos: 0,
                };
                if fields.u32()? != 1 {
                    bail!("capture helper format: expected BGRA8");
                }
                frame.nanos = fields.u64()? as i64;
                geometry(frame.width, frame.height)?;
                handle(frame.texture)?;
                handle(frame.fence)?;
                if frame.sequence == 0
                    || frame.surface == 0
                    || frame.ready == 0
                    || frame.ready == u64::MAX
                    || frame.nanos < 0
                {
                    bail!("capture helper returned invalid frame metadata");
                }
                Self::Frame(frame)
            }
            4 => {
                let sequence = fields.u64()?;
                if sequence == 0 {
                    bail!("capture helper Ack sequence must be positive");
                }
                Self::Ack(sequence)
            }
            5 => Self::Stop,
            6 => Self::Stats(IsolatedCaptureStats {
                capture: CaptureStats {
                    received: fields.u64()?,
                    delivered: fields.u64()?,
                    pool_dropped: fields.u64()?,
                    rate_skipped: fields.u64()?,
                    resize_skipped: fields.u64()?,
                },
                published: fields.u64()?,
                backpressure_dropped: fields.u64()?,
                delivered: 0,
            }),
            7 => {
                let text = std::str::from_utf8(fields.0)
                    .context("capture helper error is not UTF-8")?
                    .to_owned();
                fields.0 = &[];
                Self::Error(text)
            }
            _ => bail!("capture helper message type {kind}: unknown enum value"),
        };
        if !fields.0.is_empty() {
            bail!("capture helper message has trailing fields");
        }
        Ok(message)
    }
}

struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        if self.0.len() < N {
            bail!("capture helper message is truncated");
        }
        let (value, rest) = self.0.split_at(N);
        self.0 = rest;
        Ok(value.try_into().expect("fixed-size protocol field"))
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take()?))
    }
}
