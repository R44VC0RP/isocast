//! The upload test behind `speed_test` (and Optimize for Streaming): publish
//! to the ingest as a stream would, then send filler video for a few
//! seconds as fast as the connection takes it (up to a ceiling) and see how
//! fast it went.
//!
//! The video is a real H.264 stream that shows nothing new: one tiny SPS,
//! PPS and black IDR picture (320×180 baseline), then frames made of
//! filler-data NAL units (type 12), which decoders skip. The speed comes from the server's own
//! acknowledgements (RTMP's "bytes received", asked for every 256 KB): the
//! bytes it got over the time between the first and last. If the server
//! sends too few, the bytes the connection took in the second half of the
//! test are used instead, once the socket's buffers are full.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use isocast_protocol::stream::ServerUrl;

use super::amf::{self, Amf};
use super::flv;
use super::rtmp::Connection;

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The server acknowledges every this many bytes.
const ACK_WINDOW: u32 = 256 * 1024;
/// Each video message carries this much filler.
const FRAME_BYTES: usize = 32 * 1024;
/// How long to wait for the last acknowledgements after sending.
const DRAIN: Duration = Duration::from_millis(1500);

/// A 320×180 baseline H.264 stream's parameter sets (NAL types 7 and 8).
const SPS: [u8; 25] = [
    0x67, 0x42, 0xc0, 0x0d, 0xd9, 0x01, 0x41, 0x9f, 0x9f, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10,
    0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0xa4, 0x80,
];
const PPS: [u8; 5] = [0x68, 0xcb, 0x83, 0xcb, 0x20];
/// One 320×180 black IDR picture for them (NAL type 5), so a receiver
/// that decodes to learn the size (ffmpeg does) sees a picture.
const IDR: [u8; 194] = [
    0x65, 0x88, 0x84, 0x09, 0xf2, 0x62, 0x80, 0x00, 0xa1, 0x6c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c,
    0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9c, 0x9d, 0x75, 0xd7, 0x5d,
    0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75,
    0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7,
    0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d,
    0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75,
    0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7,
    0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d,
    0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75,
    0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7,
    0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d,
    0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75, 0xd7, 0x5d, 0x75,
    0xd7, 0x5e,
];

/// What the test measured.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    /// Upload speed, kb/s.
    pub kbps: u32,
    /// `acknowledged` or `sent` (see the module's notes).
    pub method: &'static str,
    pub seconds: f64,
    /// Bytes sent, protocol included.
    pub bytes: u64,
    /// It sent at the ceiling the whole time.
    pub capped: bool,
}

/// Publish to `server` as `key` and send filler for `seconds`, at most
/// `cap_kbps`. Errors never contain the key.
pub fn upload_test(
    server: &ServerUrl,
    key: &str,
    seconds: f64,
    cap_kbps: u32,
    stop: &AtomicBool,
) -> Result<Measured> {
    let mut connection = Connection::publish(server, key, CONNECT_TIMEOUT, stop)?;
    let result = send(&mut connection, seconds, cap_kbps);
    connection.close(key);
    result
}

fn send(connection: &mut Connection, seconds: f64, cap_kbps: u32) -> Result<Measured> {
    connection.ask_acknowledgements(ACK_WINDOW)?;
    connection.send_metadata(&amf::encode(&[
        Amf::string("@setDataFrame"),
        Amf::string("onMetaData"),
        Amf::EcmaArray(vec![
            ("width".into(), Amf::Number(320.0)),
            ("height".into(), Amf::Number(180.0)),
            ("videocodecid".into(), Amf::Number(7.0)),
            ("framerate".into(), Amf::Number(30.0)),
            (
                "encoder".into(),
                Amf::String(format!("Isocast {} upload test", env!("CARGO_PKG_VERSION"))),
            ),
        ]),
    ]))?;
    connection.send_video(0, &flv::avc_sequence_header(&decoder_configuration()))?;
    let frame = filler_frame();
    let began = Instant::now();
    let start_bytes = connection.bytes_sent;
    let mut half: Option<(Instant, u64)> = None;
    let mut keyframe = true;
    let limit = Duration::from_secs_f64(seconds);
    while began.elapsed() < limit {
        let elapsed = began.elapsed();
        if half.is_none() && elapsed >= limit / 2 {
            half = Some((Instant::now(), connection.bytes_sent));
        }
        // Keep under the ceiling.
        let sent_bits = (connection.bytes_sent - start_bytes) as f64 * 8.0;
        let allowed = f64::from(cap_kbps) * 1000.0 * elapsed.as_secs_f64();
        if sent_bits > allowed {
            std::thread::sleep(Duration::from_millis(2));
            connection.poll().context("the server stopped the test")?;
            continue;
        }
        let body = if keyframe {
            let mut nal_units = (IDR.len() as u32).to_be_bytes().to_vec();
            nal_units.extend_from_slice(&IDR);
            nal_units.extend_from_slice(&frame);
            flv::avc_frame(true, 0, &nal_units)
        } else {
            flv::avc_frame(false, 0, &frame)
        };
        keyframe = false;
        connection.send_video(elapsed.as_millis() as u32, &body)?;
        connection.poll().context("the server stopped the test")?;
    }
    let sent_at = Instant::now();
    let total = connection.bytes_sent - start_bytes;
    let elapsed = began.elapsed().as_secs_f64();
    // The last acknowledgements.
    while sent_at.elapsed() < DRAIN {
        std::thread::sleep(Duration::from_millis(20));
        if connection.poll().is_err() {
            break;
        }
    }
    let capped_rate = total as f64 * 8.0 / elapsed / 1000.0 >= f64::from(cap_kbps) * 0.95;
    if let Some(((first_at, first), (last_at, last))) = connection.acknowledgements()
        && last_at.duration_since(first_at) >= Duration::from_secs(1)
        && last > first
    {
        let span = last_at.duration_since(first_at).as_secs_f64();
        let kbps = (f64::from(last.wrapping_sub(first)) * 8.0 / span / 1000.0) as u32;
        return Ok(Measured {
            kbps,
            method: "acknowledged",
            seconds: elapsed,
            bytes: total,
            capped: capped_rate,
        });
    }
    let (half_at, half_bytes) = half.context("the test ended before it was half done")?;
    let span = sent_at.duration_since(half_at).as_secs_f64().max(0.001);
    let kbps = ((connection.bytes_sent - half_bytes) as f64 * 8.0 / span / 1000.0) as u32;
    Ok(Measured {
        kbps,
        method: "sent",
        seconds: elapsed,
        bytes: total,
        capped: capped_rate,
    })
}

/// The AVCDecoderConfigurationRecord for [`SPS`] and [`PPS`].
fn decoder_configuration() -> Vec<u8> {
    let mut record = vec![1, SPS[1], SPS[2], SPS[3], 0xff, 0xe1];
    record.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
    record.extend_from_slice(&SPS);
    record.push(1);
    record.extend_from_slice(&(PPS.len() as u16).to_be_bytes());
    record.extend_from_slice(&PPS);
    record
}

/// One frame's NAL units: a filler-data NAL (type 12) of `FRAME_BYTES`,
/// after its 4-byte length.
fn filler_frame() -> Vec<u8> {
    let mut nal = vec![0x0c];
    nal.resize(FRAME_BYTES - 1, 0xff);
    nal.push(0x80);
    let mut out = (nal.len() as u32).to_be_bytes().to_vec();
    out.extend(nal);
    out
}
