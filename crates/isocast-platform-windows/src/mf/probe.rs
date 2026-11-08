//! Decode just the probe IDR, independently of the encoder and its D3D
//! device. A synchronous MF decoder gives us CPU rows, not a second encode
//! session, a temporary movie, or a full-frame RGB readback.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::GUID;

use super::{Activation, Runtime};
use crate::encoder::Parameters;
use crate::media::Locked;
use crate::native::Apartment;
use crate::{Frame, GraphicsDevice, PixelFormat, TexturePool};

/// Lives only for one serialized batch, on one device. A recovery open or
/// doctor invocation gets its own batch. MF probes run on their own MTA.
#[derive(Default)]
pub(crate) struct Batch {
    pattern: Option<Frame>,
    checks: Arc<parking_lot::Mutex<Checks>>,
}

#[derive(Clone)]
pub(crate) struct Picture {
    pub frame: Frame,
    checks: Arc<parking_lot::Mutex<Checks>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Nvenc {
        preset: u32,
        qp: u32,
    },
    Mf {
        class: GUID,
        hardware: bool,
        quality: Option<u32>,
    },
}

struct Verified {
    provider: Provider,
    fps: u32,
    input: PixelFormat,
    config: Vec<u8>, // complete avcC/hvcC: codec, profile, depth and SPS size
    idr: Vec<u8>,
}

#[derive(Default)]
struct Checks {
    verified: Vec<Verified>,
    missing_decoders: Vec<GUID>, // Only an actually empty enumeration, not a negotiation failure.
}

impl Batch {
    pub(crate) fn picture(
        &mut self,
        device: &GraphicsDevice,
        w: u32,
        h: u32,
        input: PixelFormat,
    ) -> Result<Picture> {
        let frame = match &self.pattern {
            Some(f)
                if device.owns(f) && (f.width(), f.height(), f.pixel_format()) == (w, h, input) =>
            {
                f.clone()
            }
            _ => {
                let f = TexturePool::new(device.clone()).probe_frame(
                    w,
                    h,
                    input,
                    crate::encoder::PROBE_BARS,
                )?;
                self.pattern = Some(f.clone());
                f
            }
        };
        Ok(Picture {
            frame,
            checks: self.checks.clone(),
        })
    }
}

impl Picture {
    #[cfg(debug_assertions)]
    pub(crate) fn with_frame(&self, frame: Frame) -> Self {
        Self {
            frame,
            checks: self.checks.clone(),
        }
    }

    /// Reuse a positive result only for byte-identical decoder input. This
    /// also detects a bad session that opened at the same settings as a good
    /// one but emitted a different first picture. Never cache a rejection.
    pub(crate) fn check(
        &self,
        params: &Parameters,
        idr: &[u8],
        fps: u32,
        provider: Option<Provider>,
    ) -> Result<Option<String>> {
        let config = params.probe_config(&self.frame)?; // checked for *every* encoder
        let input = self.frame.pixel_format();
        check_once(&self.checks, provider, input, fps, config, idr, || {
            qualification(inspect(params, idr, &self.frame, fps, &self.checks))
                .context("encoder preflight: decode probe IDR")
        })
    }
}

fn check_once(
    checks: &parking_lot::Mutex<Checks>,
    provider: Option<Provider>,
    input: PixelFormat,
    fps: u32,
    config: Vec<u8>,
    idr: &[u8],
    decode: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<String>> {
    if let Some(p) = provider
        && checks.lock().verified.iter().any(|v| {
            v.provider == p
                && v.fps == fps
                && v.input == input
                && v.config == config
                && v.idr == idr
        })
    {
        let _hit = isocast_diagnostics::gpu_startup_span("encoder preflight identical IDR");
        return Ok(None);
    }
    let _timing = isocast_diagnostics::gpu_startup_span("encoder preflight IDR decode");
    let result = decode()?;
    // Flat bars normally take a few KiB. Unusual/large results still get
    // checked, without growing the batch's temporary memory unboundedly.
    if result.is_none()
        && let Some(provider) = provider
        && config.len() + idr.len() <= 512 * 1024
    {
        let mut checks = checks.lock();
        if checks.verified.len() < 8 {
            checks.verified.push(Verified {
                provider,
                fps,
                input,
                config,
                idr: idr.to_vec(),
            });
        }
    }
    Ok(result)
}

/// Only explicit decoder availability/negotiation limits are skippable.
/// All other errors (in particular `?` after negotiation) reject the IDR.
#[derive(Debug)]
enum Decoded {
    Valid,
    Unavailable(anyhow::Error),
}

fn qualification(result: Result<Decoded>) -> Result<Option<String>> {
    match result? {
        Decoded::Valid => Ok(None),
        Decoded::Unavailable(reason) => Ok(Some(format!("pixel check skipped: {reason:#}"))),
    }
}

fn inspect(
    params: &Parameters,
    idr: &[u8],
    frame: &Frame,
    fps: u32,
    checks: &parking_lot::Mutex<Checks>,
) -> Result<Decoded> {
    let (codec, subtype, ten_bit, arrays): (_, _, _, &[&[Vec<u8>]]) = match params {
        Parameters::H264 { sets, .. } => {
            ("H.264", MFVideoFormat_H264, false, &[&sets.sps, &sets.pps])
        }
        Parameters::Hevc { sets, depth } => (
            "HEVC",
            MFVideoFormat_HEVC,
            *depth == 10,
            &[&sets.vps, &sets.sps, &sets.pps],
        ),
    };
    let missing = || {
        Decoded::Unavailable(anyhow::anyhow!(
            "no synchronous Media Foundation {codec} decoder installed; install the Media Feature Pack or HEVC Video Extensions"
        ))
    };
    if checks.lock().missing_decoders.contains(&subtype) {
        return Ok(missing());
    }
    // Only touch COM when this codec actually needs a decoder lookup.
    let (_apartment, _runtime) = match Apartment::new().and_then(|a| Ok((a, Runtime::new()?))) {
        Ok(guards) => guards,
        Err(e) => {
            return Ok(Decoded::Unavailable(
                e.context("starting the MF decoder runtime"),
            ));
        }
    };
    let decoders = decoders(subtype)?;
    if decoders.is_empty() {
        checks.lock().missing_decoders.push(subtype);
        return Ok(missing());
    }
    let mut bytes = Vec::with_capacity(idr.len());
    for nal in arrays
        .iter()
        .flat_map(|sets| sets.iter().map(Vec::as_slice))
        .chain(isocast_protocol::h264::nal_units(idr))
    {
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(nal);
    }
    let sample = unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len().try_into()?)?;
        let mut at = std::ptr::null_mut();
        buffer.Lock(&mut at, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(0)?;
        sample.SetSampleDuration(10_000_000 / i64::from(fps))?;
        sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        sample
    };
    let attempts = decoders.into_iter().map(|decoder| {
        let activation = Activation(decoder, None);
        let decoder: IMFTransform = match unsafe { activation.0.ActivateObject() } {
            Ok(decoder) => decoder,
            Err(e) => {
                return Ok(Decoded::Unavailable(
                    anyhow::Error::from(e).context("decoder activation"),
                ));
            }
        };
        decode(&decoder, subtype, ten_bit, &sample, frame, fps)
    });
    try_decoders(attempts, codec, (frame.width(), frame.height()))
}

fn try_decoders(
    attempts: impl Iterator<Item = Result<Decoded>>,
    codec: &str,
    (width, height): (u32, u32),
) -> Result<Decoded> {
    let mut failures = Vec::new();
    for attempt in attempts {
        match attempt? {
            Decoded::Valid => return Ok(Decoded::Valid),
            Decoded::Unavailable(limit) => failures.push(format!("{limit:#}")),
        }
    }
    Ok(Decoded::Unavailable(anyhow::anyhow!(
        "no usable {codec} decoder for {width}x{height}: {}",
        failures.join("; ")
    )))
}

fn decoders(subtype: GUID) -> Result<Vec<IMFActivate>> {
    #[cfg(debug_assertions)]
    if std::env::var_os("ISOCAST_TEST_NO_PROBE_DECODER").is_some() {
        return Ok(Vec::new());
    }
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let (mut list, mut count) = (std::ptr::null_mut(), 0);
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT
                | MFT_ENUM_FLAG_SORTANDFILTER
                | MFT_ENUM_FLAG_LOCALMFT
                | MFT_ENUM_FLAG_UNTRUSTED_STOREMFT,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
    }?;
    let mut choices = unsafe { super::take_activations(list, count) };
    if subtype == MFVideoFormat_HEVC {
        // Stable: preserve MF's relative preference for the alternatives.
        choices.sort_by_key(|a| {
            unsafe { a.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok()
                != Some(CLSID_MSH265DecoderMFT)
        });
    }
    Ok(choices)
}

/// NOTACCEPTING is backpressure: pull output before retrying the same IDR.
/// A different ProcessInput failure on negotiated media types is terminal.
fn input_accepted(result: windows::core::Result<()>) -> Result<bool> {
    match result {
        Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
        other => other
            .map(|()| true)
            .context("negotiated decoder refused the probe input sample"),
    }
}

fn decode(
    decoder: &IMFTransform,
    subtype: GUID,
    ten_bit: bool,
    sample: &IMFSample,
    frame: &Frame,
    fps: u32,
) -> Result<Decoded> {
    let (w, h) = (frame.width(), frame.height());
    unsafe {
        let ty = MFCreateMediaType()?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        ty.SetUINT64(&MF_MT_FRAME_SIZE, u64::from(w) << 32 | u64::from(h))?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, u64::from(fps) << 32 | 1)?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if let Err(e) = decoder
            .SetInputType(0, &ty, 0)
            .context("decoder refused the probe input type")
        {
            return Ok(Decoded::Unavailable(e));
        }
        // Main10 may initially advertise NV12 before it sees the SPS. Either
        // output suffices for this luma check; the SPS check enforces depth.
        let mut layout = match output_type(decoder, ten_bit) {
            Ok(layout) => layout,
            Err(e) => {
                return Ok(Decoded::Unavailable(
                    e.context("decoder output negotiation"),
                ));
            }
        };
        decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        let mut accepted = false;
        for _ in 0..4 {
            if !accepted && input_accepted(decoder.ProcessInput(0, sample, 0))? {
                accepted = true;
                decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                decoder.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
            }
            let info = decoder.GetOutputStreamInfo(0)?;
            let (coded_w, coded_h, p010, stride) = layout;
            ensure!(
                coded_w >= w && coded_h >= h && coded_w <= 16384 && coded_h <= 16384,
                "invalid decoded size {coded_w}x{coded_h} for {w}x{h}"
            );
            let ours = if info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                == 0
            {
                let ours = MFCreateSample()?;
                let format = if p010 {
                    MFVideoFormat_P010
                } else {
                    MFVideoFormat_NV12
                };
                ours.AddBuffer(&MFCreate2DMediaBuffer(
                    coded_w,
                    coded_h,
                    format.data1,
                    false,
                )?)?;
                Some(ours)
            } else {
                None
            };
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(ours),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let result = decoder.ProcessOutput(0, std::slice::from_mut(&mut out), &mut 0);
            let picture = ManuallyDrop::take(&mut out.pSample);
            drop(ManuallyDrop::take(&mut out.pEvents));
            match result {
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    layout = match output_type(decoder, ten_bit) {
                        Ok(layout) => layout,
                        Err(e) => {
                            return Ok(Decoded::Unavailable(
                                e.context("decoder output renegotiation"),
                            ));
                        }
                    };
                }
                Err(e)
                    if e.code() == MF_E_NOTACCEPTING
                        || (!accepted && e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT) =>
                {
                    continue;
                }
                Err(e) => {
                    return Err(e)
                        .context("negotiated decoder produced no picture from the probe IDR");
                }
                Ok(()) => {
                    if !accepted {
                        continue;
                    } // Not output for our IDR. Resubmit.
                    let picture = picture.context("negotiated decoder returned no picture")?;
                    if out.dwStatus & MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32
                        == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32
                        || picture.GetBufferCount()? == 0
                    {
                        bail!("negotiated decoder returned an empty IDR sample");
                    }
                    let lock = Locked::new(picture.GetBufferByIndex(0)?, stride)?;
                    for y in [h / 4, h / 2, h * 3 / 4] {
                        check_row(
                            lock.row(y as usize, w as usize * if p010 { 2 } else { 1 })?,
                            w as usize,
                            p010,
                            frame.pixel_format(),
                        )?;
                    }
                    return Ok(Decoded::Valid);
                }
            }
        }
    }
    bail!(
        "negotiated probe decoder made no progress after four output attempts (format changes or backpressure)"
    )
}

fn output_type(decoder: &IMFTransform, ten_bit: bool) -> Result<(u32, u32, bool, i32)> {
    unsafe {
        let mut nv12 = None;
        let mut p010 = None;
        for index in 0..64 {
            let ty = match decoder.GetOutputAvailableType(0, index) {
                Ok(ty) => ty,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(e.into()),
            };
            let subtype = ty.GetGUID(&MF_MT_SUBTYPE)?;
            if subtype == MFVideoFormat_NV12 {
                nv12 = Some(ty);
                if !ten_bit {
                    break;
                }
            } else if subtype == MFVideoFormat_P010 && ten_bit {
                p010 = Some(ty);
                break;
            }
        }
        let ty = p010
            .or(nv12)
            .context("decoder offers neither NV12 nor the requested P010")?;
        decoder.SetOutputType(0, &ty, 0)?;
        let current = decoder.GetOutputCurrentType(0)?;
        let p010 = current.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_P010;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let w = (size >> 32) as u32;
        let stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .unwrap_or(w.saturating_mul(if p010 { 2 } else { 1 }));
        Ok((w, size as u32, p010, stride as i32))
    }
}

/// The probe writes full-range RGB or native studio-range NV12. A decoder
/// may expose either range. Both bars must match the same interpretation.
fn check_row(row: &[u8], width: usize, p010: bool, input: PixelFormat) -> Result<()> {
    let at = |x: usize| -> u8 {
        if p010 {
            (u16::from_le_bytes([row[2 * x], row[2 * x + 1]]) >> 8) as u8
        } else {
            row[x]
        }
    };
    let measured = [at(width / 4), at(3 * width / 4)];
    let bars = crate::encoder::PROBE_BARS;
    let (limited, full) = if input == PixelFormat::Nv12 {
        (
            bars,
            bars.map(|v| ((u32::from(v.saturating_sub(16)) * 255 + 109) / 219).min(255) as u8),
        )
    } else {
        (
            bars.map(|v| (16 + (u32::from(v) * 219 + 127) / 255) as u8),
            bars,
        )
    };
    ensure!(
        [limited, full]
            .into_iter()
            .any(|pair| measured.iter().zip(pair).all(|(a, b)| a.abs_diff(b) <= 10)),
        "probe bar luma mismatch: measured {measured:?}, expected {limited:?} or {full:?} ±10 (8-bit Y)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_identical_successful_pictures_share_a_decode_within_the_batch() {
        let batch = parking_lot::Mutex::new(Checks::default());
        let nv = |preset| Some(Provider::Nvenc { preset, qp: 23 });
        let check = |provider, cfg, bytes: &[u8], outcome: Result<Option<String>>| {
            check_once(
                &batch,
                provider,
                PixelFormat::Bgra8,
                30,
                vec![cfg],
                bytes,
                || outcome,
            )
        };
        let unexpected = || Err(anyhow::anyhow!("unexpected second decode"));
        assert!(check(nv(3), 1, b"bars", Ok(None)).unwrap().is_none());
        assert!(check(nv(3), 1, b"bars", unexpected()).unwrap().is_none());
        for (provider, cfg, bytes) in [
            (nv(3), 1, &b"bad!"[..]),
            (nv(5), 1, b"bars"),
            (nv(3), 2, b"bars"),
        ] {
            assert!(check(provider, cfg, bytes, Err(anyhow::anyhow!("bad pixels"))).is_err());
            assert_eq!(
                check(
                    provider,
                    cfg,
                    bytes,
                    Ok(Some("no compatible decoder".into()))
                )
                .unwrap()
                .as_deref(),
                Some("no compatible decoder")
            );
            assert!(check(provider, cfg, bytes, unexpected()).is_err()); // Neither refusal nor skip is a verified picture.
        }
        assert!(
            check_once(
                &Default::default(),
                nv(3),
                PixelFormat::Bgra8,
                30,
                vec![1],
                b"bars",
                unexpected
            )
            .is_err()
        );
    }

    #[test]
    fn decoder_limits_warn_but_failed_pictures_reject() {
        for limit in [
            "no HEVC decoder",
            "input too large",
            "no compatible output type",
        ] {
            let note = qualification(Ok(Decoded::Unavailable(anyhow::anyhow!(limit))))
                .unwrap()
                .unwrap();
            assert!(note.contains(limit));
        }
        assert!(qualification(Ok(Decoded::Valid)).unwrap().is_none());
        for reason in ["no drained IDR", "green", "rows do not fit"] {
            assert!(qualification(Err(anyhow::anyhow!(reason))).is_err());
        }
        let process_input = |hr| input_accepted(Err(windows::core::Error::from_hresult(hr)));
        assert!(!process_input(MF_E_NOTACCEPTING).unwrap());
        let attempts = [
            Ok(Decoded::Unavailable(anyhow::anyhow!("failed activation"))),
            process_input(MF_E_INVALID_STREAM_DATA).map(|_| Decoded::Valid),
            Ok(Decoded::Valid),
        ];
        let refused =
            qualification(try_decoders(attempts.into_iter(), "H.264", (320, 180))).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("negotiated decoder refused the probe input sample")
        );
    }

    /// Exercise the HEVC bitstream + optional pixel check even on a WARP
    /// runner that has neither a native HEVC encoder nor the Store decoder.
    #[test]
    #[ignore = "requires Windows with ffmpeg/libx265"]
    fn hevc_probe_with_installed_decoders() -> Result<()> {
        use crate::encoder::{PROBE_FRAMES, check_probe_packet, probe_pts};
        let _apartment = Apartment::new()?;
        let _runtime = Runtime::new()?;
        let decoders = decoders(MFVideoFormat_HEVC)?;
        let names = decoders
            .iter()
            .map(super::super::name)
            .collect::<Result<Vec<_>>>()?;
        println!("HEVC decoder enumeration (including Store): {names:?}");
        if std::env::var_os("ISOCAST_TEST_HAS_HEVC_EXTENSION").is_some() {
            ensure!(
                !names.is_empty(),
                "installed HEVC extension was not enumerated"
            );
        }
        let run = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "nullsrc=s=320x180:r=30,geq=lum='if(lt(X,W/2),71,181)':cb=128:cr=128",
                "-frames:v",
                "3",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx265",
                "-x265-params",
                "bframes=0:aud=1:repeat-headers=1:info=0",
                "-f",
                "hevc",
                "pipe:1",
            ])
            .output()?;
        ensure!(
            run.status.success(),
            "ffmpeg: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let nals = crate::h264::annex_b(&run.stdout)?;
        let mut params = Parameters::new(
            crate::VideoEncoding::Hevc,
            isocast_protocol::api::H264Profile::High,
        );
        let mut packets = Vec::new();
        for group in nals
            .split(|nal| (nal[0] >> 1) & 63 == 35)
            .filter(|g| !g.is_empty())
        {
            let mut bytes = Vec::new();
            for nal in group {
                bytes.extend_from_slice(&[0, 0, 0, 1]);
                bytes.extend_from_slice(nal);
            }
            if let Some(packet) = params.packet(&bytes, probe_pts(packets.len(), 30))? {
                check_probe_packet(&packet, packets.len(), 30)?;
                packets.push(packet);
            }
        }
        assert_eq!(packets.len(), PROBE_FRAMES);
        let picture =
            Batch::default().picture(&GraphicsDevice::new()?, 320, 180, PixelFormat::Bgra8)?;
        let note = picture.check(&params, &packets[0].data, 30, None)?;
        println!(
            "HEVC preflight accepted 3 pictures: {}",
            note.as_deref().unwrap_or("luma checked")
        );
        if names.is_empty() {
            assert!(
                note.unwrap()
                    .contains("no synchronous Media Foundation HEVC decoder")
            );
        }
        Ok(())
    }
    #[test]
    fn luma_checks_both_bars_in_negotiated_range_and_depth() {
        assert!(check_row(&[61, 61, 191, 191], 4, false, PixelFormat::Bgra8).is_ok());
        assert!(check_row(&[64, 64, 192, 192], 4, false, PixelFormat::Bgra8).is_ok());
        assert!(check_row(&[56, 56, 205, 205], 4, false, PixelFormat::Nv12).is_ok());
        assert!(check_row(&[64, 64, 192, 192], 4, false, PixelFormat::Nv12).is_ok());
        assert!(
            check_row(
                &[0, 71, 64, 71, 128, 181, 192, 181],
                4,
                true,
                PixelFormat::Rgba10
            )
            .is_ok()
        );
        for bad in [[16; 4], [144; 4], [181, 181, 71, 71], [71, 71, 170, 170]] {
            let err = check_row(&bad, 4, false, PixelFormat::Bgra8).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("measured [{}, {}]", bad[1], bad[3]))
            );
        }
    }
}
