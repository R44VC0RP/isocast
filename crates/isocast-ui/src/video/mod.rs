//! Live video from another computer's engine: H.264 access units in,
//! pictures out, in three parts that don't know about each other:
//!
//! - a [`Source`] of access units: today the engine's `frames` stream
//!   over the window's authenticated connection ([`FramesSource`]); the
//!   server protocol's WebRTC path (docs/server-protocol.md: `whep_offer`
//!   on its own R1 connection, str0m receiving) will be another `Source`
//!   that turns each received Annex B frame into [`Intake`] with
//!   [`from_annex_b`], and nothing below it changes;
//! - a [`Decoder`] (VideoToolbox on macOS and Media Foundation on
//!   Windows, both hardware; OpenH264 in software on Linux) turning
//!   access units into pictures: on macOS and Windows on the GPU where it
//!   can ([`Decoded::Frame`], shown as the local engine's shared frames
//!   are), else (and always on Linux) RGBA pixels;
//! - the window's monitors, which show each picture as they show the
//!   local engine's frames (`frames::Incoming::Frame`) or the engine's
//!   stills (`frames::Incoming::Still`).
//!
//! Access units are AVCC: each NAL unit after its length (4 bytes, big
//! endian), as MP4 and the `frames` records carry them. The parameter sets
//! (SPS and PPS) come separately, whenever they change, before the
//! keyframe that needs them.

use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::api::{FrameRate, FramesParams, frames};
use slint::{Rgba8Pixel, SharedPixelBuffer};

#[cfg(windows)]
mod dxva_frames;
#[cfg(windows)]
mod media_foundation;
#[cfg(target_os = "linux")]
mod openh264;
#[cfg(target_os = "linux")]
mod pw_loop;
pub mod sound;
pub mod sync;
#[cfg(any(windows, target_os = "macos"))]
pub mod uploads;
#[cfg(target_os = "macos")]
mod videotoolbox;
mod whep;

pub use whep::WhepSource;

/// The widths a remote window asks for (the widest is enough for a
/// monitor and cheap on the network, about 2 Mb/s at 30 fps): a monitor
/// gets the step that covers it, so dragging its size reconnects only when
/// it crosses a step.
pub const WIDTHS: [u32; 3] = [480, 640, 960];

/// The width on [`WIDTHS`]' ladder to ask for a monitor `width` pixels
/// wide.
pub fn width_step(width: u32) -> u32 {
    WIDTHS
        .into_iter()
        .find(|step| width <= *step)
        .unwrap_or(WIDTHS[WIDTHS.len() - 1])
}

#[cfg(any(windows, target_os = "linux"))]
pub use isocast_protocol::h264::nal_units;
/// H.264 framing (untrusted bytes from the engine), parsed in the
/// protocol crate where its fuzz target reaches it.
pub use isocast_protocol::h264::{ParameterSets, annex_b_units};

/// One coded picture.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// AVCC: each NAL unit after its 4-byte big-endian length.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// When the engine drew it, on the engine's clock, in nanoseconds.
    pub time: u64,
    /// The picture's size (the parameter sets say it too); 0 if the source
    /// doesn't know it.
    pub width: u32,
    pub height: u32,
}

/// An Annex B access unit (start codes, parameter sets in band before
/// IDRs, as WebRTC receivers hand H.264 over) as this module's intake: the
/// parameter sets it carries, if any (send them as [`Intake::Config`]
/// first when they changed), and the picture as AVCC. Its size isn't
/// parsed from the SPS: `width` and `height` are 0 (unknown), and the
/// decoders use the decoded picture's size.
pub fn from_annex_b(data: &[u8], time: u64) -> (Option<ParameterSets>, AccessUnit) {
    let mut sets = ParameterSets::default();
    let mut avcc = Vec::with_capacity(data.len());
    let mut keyframe = false;
    for nal in annex_b_units(data) {
        match nal.first().map(|header| header & 0x1f) {
            Some(7) => sets.sps.push(nal.to_vec()),
            Some(8) => sets.pps.push(nal.to_vec()),
            // Access unit delimiters carry nothing a decoder needs.
            Some(9) | None => {}
            Some(kind) => {
                keyframe |= kind == 5;
                avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                avcc.extend_from_slice(nal);
            }
        }
    }
    let sets = (!sets.sps.is_empty() && !sets.pps.is_empty()).then_some(sets);
    let unit = AccessUnit {
        data: avcc,
        keyframe,
        time,
        width: 0,
        height: 0,
    };
    (sets, unit)
}

/// What a source delivers, in order.
#[derive(Debug, Clone)]
pub enum Intake {
    /// New parameter sets: they apply from the next keyframe.
    Config(ParameterSets),
    Unit(AccessUnit),
    /// The pictures from `time` on were drawn from setup `revision` (the
    /// program and the preview only), so the selection box can follow.
    Revision {
        time: u64,
        revision: u64,
    },
}

/// Where access units come from.
pub trait Source: Send {
    /// The next item, or `None` if nothing came within `wait` (the picture
    /// may not have changed). An error ends the source.
    fn next(&mut self, wait: Duration) -> Result<Option<Intake>>;
    /// Ask for at most `fps` pictures a second from now on.
    fn set_fps(&mut self, fps: u32) -> Result<()>;
    /// The engine's clock minus this one's, in nanoseconds, if known: how
    /// late pictures arrive.
    fn clock_offset(&self) -> Option<i128> {
        None
    }
}

/// A decoded picture, ready for a monitor.
pub type Picture = SharedPixelBuffer<Rgba8Pixel>;

/// A decoded picture: still on the GPU, in a surface the window's
/// renderer reads in place, or RGBA pixels.
pub enum Decoded {
    /// Not on Linux: its decoder works in software and the window shows
    /// Slint images.
    #[cfg_attr(
        target_os = "linux",
        expect(dead_code, reason = "macOS and Windows only")
    )]
    Frame(isocast_client::shared::SharedFrame),
    Pixels(Picture),
}

impl Decoded {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Decoded::Frame(frame) => (frame.width(), frame.height()),
            Decoded::Pixels(pixels) => (pixels.width(), pixels.height()),
        }
    }
}

/// Turns access units into pictures (hardware where the OS has it;
/// OpenH264 in software on Linux).
pub trait Decoder {
    /// Decode `unit`, coded with `sets`: its picture, or none (the decoder
    /// is waiting for a keyframe, holds the picture back, or has nowhere
    /// free to put it). With `surfaces` (the monitor shows them in place:
    /// native layers) the picture stays on the GPU where the decoder can
    /// keep it there; else, or where it can't, it comes as RGBA pixels.
    /// (macOS's surfaces are BGRA, which Slint images don't take.)
    fn decode(
        &mut self,
        sets: &ParameterSets,
        unit: &AccessUnit,
        surfaces: bool,
    ) -> Result<Option<Decoded>>;
    /// What decodes, for diagnostics ("VideoToolbox (hardware)").
    fn name(&self) -> String;
    /// Pictures decoded but skipped since last asked: every surface they
    /// could go in was still in use.
    fn take_skipped(&mut self) -> u32 {
        0
    }
    /// The H.264 profile to ask a `frames` stream for: none for the
    /// engine's default (High), which hardware decoders take.
    fn frames_profile(&self) -> Option<isocast_protocol::api::H264Profile> {
        None
    }
}

/// No decoder can decode the stream on this machine at all (Windows N
/// without the Media Feature Pack, an OS without one): asking again won't
/// help, so the window shows the engine's stills instead.
#[derive(Debug)]
pub struct NoDecoder(pub String);

impl std::fmt::Display for NoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoDecoder {}

/// Whether `error` is (or wraps) a [`NoDecoder`].
pub fn no_decoder(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NoDecoder>().is_some()
}

/// This OS's decoder.
pub fn decoder() -> Result<Box<dyn Decoder>> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(videotoolbox::VideoToolbox::default()));
    #[cfg(windows)]
    return Ok(Box::new(media_foundation::MediaFoundation::new()?));
    // Cisco's library, as installed: without it the window shows the
    // engine's stills, saying why (and how to install it).
    #[cfg(target_os = "linux")]
    return match isocast_openh264::check() {
        Ok(_) => Ok(Box::new(openh264::OpenH264::default())),
        Err(error) => Err(NoDecoder(error.to_string()).into()),
    };
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    Err(NoDecoder("no H.264 decoder on this OS".into()).into())
}

/// The engine's `frames` stream: low-latency H.264 from its hardware
/// encoder, on a connection of its own (TLS for another computer's
/// engine). A thread reads records so a still picture (no records) never
/// blocks the window's checks.
pub struct FramesSource {
    records: Receiver<std::io::Result<frames::Record>>,
    writer: isocast_ipc::Stream,
    offset: i128,
    /// The window's link generation this stream belongs to (none while the
    /// link wasn't up yet when it opened).
    link: Option<u64>,
}

impl FramesSource {
    /// Start `params`' stream on `client`'s connection.
    pub fn open(client: isocast_client::Client, params: FramesParams) -> Result<Self> {
        let asked = Instant::now();
        let (reply, mut reader) = client.frames(params)?;
        if reply.codec != "h264" {
            bail!("the engine sends {} video, not H.264", reply.codec);
        }
        // The engine read its clock between the ask and the reply: assume
        // halfway (a few milliseconds on a LAN).
        let halfway = asked + asked.elapsed() / 2;
        let offset = reply.host_time as i128 - crate::video::now_nanos(halfway) as i128;
        let writer = reader.get_ref().try_clone()?;
        reader.get_ref().set_read_timeout(None)?;
        // No read timeout (the call's own 30 s is cleared): a still picture
        // sends nothing for as long as it
        // stays still, and a record cut by a timeout would lose the stream's
        // framing. A dead link (a pulled cable) is the window's to notice
        // (`crate::daemon::link`); `next` then ends the stream.
        let (tx, records) = sync_channel(8);
        thread::Builder::new()
            .name("remote video records".into())
            .spawn(move || {
                loop {
                    let record = match frames::Record::read(&mut reader) {
                        Ok(Some(record)) => Ok(record),
                        Ok(None) => Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "the engine ended the video stream",
                        )),
                        Err(error) => Err(error),
                    };
                    let end = record.is_err();
                    if tx.send(record).is_err() || end {
                        return;
                    }
                }
            })
            .context("could not start the video reader")?;
        let (up, generation) = crate::daemon::link();
        Ok(Self {
            records,
            writer,
            offset,
            link: up.then_some(generation),
        })
    }
}

impl Drop for FramesSource {
    fn drop(&mut self) {
        // Ends the reader thread's wait, and the engine's stream.
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }
}

impl Source for FramesSource {
    fn next(&mut self, wait: Duration) -> Result<Option<Intake>> {
        let record = match self.records.recv_timeout(wait) {
            Ok(record) => record.context("lost the video stream")?,
            // Nothing new (the picture may be still): fine while the
            // window's link to the engine is the one this stream began on.
            Err(RecvTimeoutError::Timeout) => {
                let (up, generation) = crate::daemon::link();
                match self.link {
                    None if up => self.link = Some(generation),
                    Some(began) if !up || generation != began => {
                        bail!("lost the link to the engine")
                    }
                    _ => {}
                }
                return Ok(None);
            }
            Err(RecvTimeoutError::Disconnected) => bail!("lost the video stream"),
        };
        Ok(Some(match record.kind {
            frames::CONFIG => Intake::Config(ParameterSets::from_avcc(&record.payload)?),
            frames::KEY_FRAME | frames::FRAME => Intake::Unit(AccessUnit {
                data: record.payload,
                keyframe: record.kind == frames::KEY_FRAME,
                time: record.seq,
                width: record.width,
                height: record.height,
            }),
            frames::REVISION if record.payload.len() >= 8 => Intake::Revision {
                time: record.seq,
                revision: u64::from_be_bytes(record.payload[..8].try_into()?),
            },
            frames::END => bail!(
                "the engine ended the video stream: {}",
                String::from_utf8_lossy(&record.payload)
            ),
            // Kinds a newer engine may add.
            _ => return Ok(None),
        }))
    }

    fn set_fps(&mut self, fps: u32) -> Result<()> {
        let line = serde_json::to_string(&FrameRate {
            fps: fps.clamp(1, 60),
        })?;
        writeln!(self.writer, "{line}")?;
        Ok(())
    }

    fn clock_offset(&self) -> Option<i128> {
        Some(self.offset)
    }
}

/// This machine's clock in nanoseconds since an arbitrary start, at `at`.
pub fn now_nanos(at: Instant) -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    at.saturating_duration_since(*start).as_nanos() as u64
}

/// How a stream of decoded pictures went: how many, how late (arrival
/// after the engine drew them, offset-corrected) and how long decoding
/// took, written to the diagnostics log every few seconds.
#[derive(Default)]
pub struct Stats {
    pictures: u32,
    skipped: u32,
    lates: Vec<f64>,
    decodes: Vec<f64>,
    since: Option<Instant>,
}

impl Stats {
    /// Pictures skipped for want of a free surface (logged even if none
    /// is shown).
    pub fn skip(&mut self, pictures: u32) {
        self.since.get_or_insert_with(Instant::now);
        self.skipped += pictures;
        self.log_when_due();
    }

    pub fn add(&mut self, late_ms: Option<f64>, decode_ms: f64) {
        self.since.get_or_insert_with(Instant::now);
        self.pictures += 1;
        self.lates.extend(late_ms);
        self.decodes.push(decode_ms);
        self.log_when_due();
    }

    fn log_when_due(&mut self) {
        let Some(since) = self.since else { return };
        if since.elapsed() < Duration::from_secs(5) {
            return;
        }
        let seconds = since.elapsed().as_secs_f64();
        let median = |values: &mut Vec<f64>| {
            values.sort_by(f64::total_cmp);
            values.get(values.len() / 2).copied().unwrap_or(f64::NAN)
        };
        let line = format!(
            "remote video: {:.1} fps ({} skipped, no free surface), arrival {:.1} ms after drawn (median), decode {:.2} ms (median)",
            f64::from(self.pictures) / seconds,
            self.skipped,
            median(&mut self.lates),
            median(&mut self.decodes),
        );
        isocast_diagnostics::diagnostic!("debug"; "{line}");
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_snap_to_the_ladder() {
        assert_eq!(width_step(16), 480);
        assert_eq!(width_step(480), 480);
        assert_eq!(width_step(481), 640);
        assert_eq!(width_step(700), 960);
        assert_eq!(width_step(4000), 960);
    }

    #[test]
    fn annex_b_becomes_config_and_avcc() {
        // AUD, SPS, PPS (3-byte start code), IDR slice.
        let idr = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 9, 0, 0, 0, 1, 0x65, 7,
            0,
        ];
        let (sets, unit) = from_annex_b(&idr, 42);
        let sets = sets.unwrap();
        assert_eq!(sets.sps, vec![vec![0x67, 1, 2]]);
        assert_eq!(sets.pps, vec![vec![0x68, 9]]);
        assert!(unit.keyframe);
        assert_eq!(unit.time, 42);
        // A trailing zero byte inside the last unit is kept.
        assert_eq!(unit.data, [0, 0, 0, 3, 0x65, 7, 0]);

        let (sets, unit) = from_annex_b(&[0, 0, 1, 0x41, 5], 43);
        assert!(sets.is_none());
        assert!(!unit.keyframe);
        assert_eq!(unit.data, [0, 0, 0, 2, 0x41, 5]);
    }
}
