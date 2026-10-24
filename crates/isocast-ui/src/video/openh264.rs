//! H.264 decoded in software with Cisco's OpenH264 binary (Linux), the one
//! the installer downloads and the Linux engine encodes with, loaded by
//! `isocast-openh264`. Each
//! access unit (AVCC) goes in as Annex B, with the parameter sets in front
//! of a keyframe whenever they changed; the decoded picture comes out as
//! RGBA for the monitors, converted as Rec.709 limited range (what the
//! Linux engine encodes and signals in its VUI; OpenH264's own conversion
//! is BT.601, which shifts colours).

use anyhow::{Context, Result, bail};
use isocast_openh264::openh264;
use openh264::decoder::{DecodeOptions, Decoder as Codec, DecoderConfig};
use openh264::formats::YUVSource;

use super::{AccessUnit, Decoded, Decoder, ParameterSets, Picture, nal_units};

/// The largest picture side decoded: the parameter sets come from the
/// engine (another computer's, for a remote one), and say how much the
/// decoder allocates.
const MAX_SIDE: u32 = 4096;

#[derive(Default)]
pub struct OpenH264 {
    codec: Option<Codec>,
    /// The parameter sets the decoder was last given.
    sets: Option<ParameterSets>,
    annex_b: Vec<u8>,
}

/// `unit` (one NAL unit) after an Annex B start code.
fn push_unit(out: &mut Vec<u8>, unit: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(unit);
}

impl Decoder for OpenH264 {
    fn name(&self) -> String {
        "OpenH264 (software)".into()
    }

    /// OpenH264 smears High (CABAC with 8x8 transforms): Constrained
    /// Baseline, as the WebRTC offer asks.
    fn frames_profile(&self) -> Option<isocast_protocol::api::H264Profile> {
        Some(isocast_protocol::api::H264Profile::ConstrainedBaseline)
    }

    /// Linux shows pictures as Slint images: always pixels.
    fn decode(
        &mut self,
        sets: &ParameterSets,
        unit: &AccessUnit,
        _surfaces: bool,
    ) -> Result<Option<Decoded>> {
        Ok(self.picture(sets, unit)?.map(Decoded::Pixels))
    }
}

impl OpenH264 {
    /// `unit`'s picture as RGBA, if it completes one.
    fn picture(&mut self, sets: &ParameterSets, unit: &AccessUnit) -> Result<Option<Picture>> {
        let fresh = self.sets.as_ref() != Some(sets);
        if fresh && !unit.keyframe {
            // New parameter sets need a keyframe to start from.
            return Ok(None);
        }
        if fresh {
            // Before OpenH264 allocates for them.
            for sps in &sets.sps {
                let (width, height) = sps_size(sps).context("unreadable H.264 SPS")?;
                if width > MAX_SIDE || height > MAX_SIDE {
                    bail!("H.264 picture {width}x{height} is larger than {MAX_SIDE}x{MAX_SIDE}");
                }
            }
        }
        if self.codec.is_none() {
            let api =
                isocast_openh264::api().map_err(|error| super::NoDecoder(error.to_string()))?;
            self.codec = Some(
                Codec::with_api_config(api, DecoderConfig::new())
                    .context("OpenH264 could not start a decoder")?,
            );
        }
        self.annex_b.clear();
        if fresh {
            for set in sets.sps.iter().chain(&sets.pps) {
                push_unit(&mut self.annex_b, set);
            }
        }
        for nal in nal_units(&unit.data) {
            push_unit(&mut self.annex_b, nal);
        }
        let codec = self.codec.as_mut().expect("made above");
        let decoded = match codec.decode_with_options(&self.annex_b, DecodeOptions::default()) {
            Ok(decoded) => decoded,
            Err(error) => {
                // Start again at the next keyframe, with a new decoder.
                self.codec = None;
                self.sets = None;
                return Err(anyhow::anyhow!(
                    "OpenH264 could not decode a picture: {error}"
                ));
            }
        };
        if fresh {
            self.sets = Some(sets.clone());
        }
        let Some(yuv) = decoded else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        if width > MAX_SIDE as usize || height > MAX_SIDE as usize {
            bail!("H.264 picture {width}x{height} is larger than {MAX_SIDE}x{MAX_SIDE}");
        }
        let mut picture = Picture::new(width as u32, height as u32);
        rec709_to_rgba(
            [yuv.y(), yuv.u(), yuv.v()],
            yuv.strides(),
            (width, height),
            picture.make_mut_bytes(),
        );
        Ok(Some(picture))
    }
}

/// 4:2:0 Rec.709 limited range (Y 16-235, Cb/Cr 16-240) to RGBA, in 16.16
/// fixed point. `out` holds `width`×`height` RGBA pixels; planes too short
/// for their strides leave the rest of `out` as it was.
fn rec709_to_rgba(
    [y, u, v]: [&[u8]; 3],
    (y_stride, u_stride, v_stride): (usize, usize, usize),
    (width, height): (usize, usize),
    out: &mut [u8],
) {
    // 255/219 and the BT.709 matrix scaled by 255/224, times 65536.
    const Y: i32 = 76_309;
    const R_V: i32 = 117_489;
    const G_U: i32 = 13_975;
    const G_V: i32 = 34_925;
    const B_U: i32 = 138_438;
    let clamp = |value: i32| ((value + 32_768) >> 16).clamp(0, 255) as u8;
    for (row, out_row) in out.chunks_exact_mut(width * 4).take(height).enumerate() {
        let (Some(y_row), Some(u_row), Some(v_row)) = (
            y.get(row * y_stride..row * y_stride + width),
            u.get(row / 2 * u_stride..row / 2 * u_stride + width.div_ceil(2)),
            v.get(row / 2 * v_stride..row / 2 * v_stride + width.div_ceil(2)),
        ) else {
            return;
        };
        for (x, pixel) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let luma = (i32::from(y_row[x]) - 16) * Y;
            let cb = i32::from(u_row[x / 2]) - 128;
            let cr = i32::from(v_row[x / 2]) - 128;
            pixel[0] = clamp(luma + R_V * cr);
            pixel[1] = clamp(luma - G_U * cb - G_V * cr);
            pixel[2] = clamp(luma + B_U * cb);
            pixel[3] = 255;
        }
    }
}

/// The picture size an SPS (a NAL unit with its header byte) codes, in
/// whole macroblocks (before cropping): enough to refuse an oversized one.
/// `None` if it ends early.
fn sps_size(sps: &[u8]) -> Option<(u32, u32)> {
    let mut bits = Bits::new(sps.get(1..)?);
    let profile = bits.bits(8)?;
    bits.bits(16)?; // constraint flags, level
    bits.ue()?; // seq_parameter_set_id
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = bits.ue()?;
        if chroma == 3 {
            bits.bits(1)?; // separate_colour_plane_flag
        }
        bits.ue()?; // bit_depth_luma_minus8
        bits.ue()?; // bit_depth_chroma_minus8
        bits.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.bits(1)? == 1 {
            for list in 0..if chroma == 3 { 12 } else { 8 } {
                if bits.bits(1)? == 1 {
                    let size = if list < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + bits.se()? + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            bits.bits(1)?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue()?.min(255) {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue()?; // max_num_ref_frames
    bits.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = bits.ue()?.checked_add(1)?;
    let height_units = bits.ue()?.checked_add(1)?;
    let frame_mbs_only = bits.bits(1)?;
    let width = width_mbs.checked_mul(16)?;
    let height = height_units.checked_mul(16 * (2 - frame_mbs_only))?;
    Some((width, height))
}

/// An RBSP bit reader (emulation prevention bytes dropped).
struct Bits {
    data: Vec<u8>,
    at: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &byte in nal {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            data.push(byte);
        }
        Self { data, at: 0 }
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = *self.data.get(self.at / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Some(value)
    }

    /// Exp-Golomb, unsigned (at most 32 bits of value).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    /// Exp-Golomb, signed.
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 {
            (code + 1) / 2
        } else {
            -code / 2
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::encoder::{Encoder, EncoderConfig};
    use openh264::formats::YUVBuffer;

    /// A `width`×`height` frame of one Rec.709 limited colour.
    fn solid(width: usize, height: usize, [y, u, v]: [u8; 3]) -> YUVBuffer {
        let luma = width * height;
        let mut planes = vec![y; luma];
        planes.resize(luma + luma / 4, u);
        planes.resize(luma + luma / 2, v);
        YUVBuffer::from_vec(planes, width, height)
    }

    /// One encoded keyframe as the decoder's intake.
    fn encode(frame: &YUVBuffer) -> (ParameterSets, AccessUnit) {
        let api = isocast_openh264::api().expect("Cisco's OpenH264 (set ISOCAST_OPENH264)");
        let mut encoder = Encoder::with_api_config(api, EncoderConfig::new()).unwrap();
        let annex_b = encoder.encode(frame).unwrap().to_vec();
        let (sets, unit) = super::super::from_annex_b(&annex_b, 0);
        (sets.expect("a keyframe carries its parameter sets"), unit)
    }

    fn near(pixel: &[u8], want: [u8; 3]) -> bool {
        pixel[..3]
            .iter()
            .zip(want)
            .all(|(&got, want)| got.abs_diff(want) <= 6)
    }

    #[test]
    fn converts_rec709_limited() {
        let convert = |[y, u, v]: [u8; 3]| {
            let mut out = [0u8; 4];
            rec709_to_rgba([&[y], &[u], &[v]], (1, 1, 1), (1, 1), &mut out);
            out
        };
        assert_eq!(convert([235, 128, 128]), [255, 255, 255, 255]);
        assert_eq!(convert([16, 128, 128]), [0, 0, 0, 255]);
        // Rec.709 red, green and blue (BT.601 would give red as ~233).
        assert!(
            near(&convert([63, 102, 240]), [255, 0, 0]),
            "{:?}",
            convert([63, 102, 240])
        );
        assert!(
            near(&convert([173, 42, 26]), [0, 255, 0]),
            "{:?}",
            convert([173, 42, 26])
        );
        assert!(
            near(&convert([32, 240, 118]), [0, 0, 255]),
            "{:?}",
            convert([32, 240, 118])
        );
    }

    #[test]
    fn converts_with_strides_and_odd_sizes() {
        // 3x3 picture, rows padded to 8 (luma) and 4 (chroma).
        let mut y = vec![0u8; 8 * 3];
        for row in 0..3 {
            y[row * 8..row * 8 + 3].fill(235);
        }
        let (u, v) = (vec![128u8; 4 * 2], vec![128u8; 4 * 2]);
        let mut out = vec![0u8; 3 * 3 * 4];
        rec709_to_rgba([&y, &u, &v], (8, 4, 4), (3, 3), &mut out);
        assert!(out.chunks(4).all(|p| p == [255, 255, 255, 255]));
    }

    #[test]
    fn decodes_solid_frames_and_follows_a_new_sps() {
        let mut decoder = OpenH264::default();
        let red = [63, 102, 240];
        let (sets, unit) = encode(&solid(64, 48, red));
        assert_eq!(sps_size(&sets.sps[0]), Some((64, 48)));
        let picture = decoder.picture(&sets, &unit).unwrap().expect("a picture");
        assert_eq!((picture.width(), picture.height()), (64, 48));
        let middle = (24 * 64 + 32) * 4;
        let bytes = picture.as_bytes();
        assert!(
            near(&bytes[middle..middle + 4], [255, 0, 0]),
            "{:?}",
            &bytes[middle..middle + 4]
        );

        // The engine changes size: new parameter sets, then a keyframe.
        let blue = [32, 240, 118];
        let (sets, unit) = encode(&solid(128, 96, blue));
        assert_eq!(sps_size(&sets.sps[0]), Some((128, 96)));
        let picture = decoder.picture(&sets, &unit).unwrap().expect("a picture");
        assert_eq!((picture.width(), picture.height()), (128, 96));
        let middle = (48 * 128 + 64) * 4;
        let bytes = picture.as_bytes();
        assert!(
            near(&bytes[middle..middle + 4], [0, 0, 255]),
            "{:?}",
            &bytes[middle..middle + 4]
        );
    }

    #[test]
    fn refuses_oversized_parameter_sets() {
        let (mut sets, unit) = encode(&solid(64, 48, [126, 128, 128]));
        // A baseline SPS of 8192x16 (512x1 macroblocks): profile 66,
        // flags, level, then sps_id 0, log2_max_frame_num 0, poc type 2,
        // 1 ref, no gaps, width_mbs-1 511, height-1 0, frame_mbs_only.
        let mut bits = String::from("1");
        bits += "1"; // log2_max_frame_num_minus4 = 0
        bits += "011"; // pic_order_cnt_type = 2
        bits += "010"; // max_num_ref_frames = 1
        bits += "0"; // gaps
        bits += &format!("{:0>19b}", 512u32); // ue(511): 9 zeros, then 512 in 10 bits
        bits += "1"; // height_minus1 = 0
        bits += "1"; // frame_mbs_only
        while bits.len() % 8 != 0 {
            bits.push('1');
        }
        let mut sps = vec![0x67, 66, 0, 30];
        sps.extend(
            bits.as_bytes()
                .chunks(8)
                .map(|byte| u8::from_str_radix(std::str::from_utf8(byte).unwrap(), 2).unwrap()),
        );
        assert_eq!(sps_size(&sps), Some((8192, 16)));
        sets.sps = vec![sps];
        let error = OpenH264::default().picture(&sets, &unit).unwrap_err();
        assert!(error.to_string().contains("8192x16"), "{error:#}");
        // An SPS cut short is refused too.
        sets.sps = vec![vec![0x67, 66]];
        assert!(OpenH264::default().picture(&sets, &unit).is_err());
    }
}
