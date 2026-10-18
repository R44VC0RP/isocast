//! The HEVC packet boundary of NVENC and Media Foundation (hvc1, no reordering).
//! Only parameter-set prefixes are parsed, not slices or VUI: the producer must
//! enforce progressive SDR709, check requested bit depth, and reject config
//! changes once the writer has frozen its sample description.

use anyhow::{Context, Result, bail};
use isocast_engine::{EncodedVideo, Time};

use crate::h264::{MAX_PACKET, annex_b};

const MAX_NALS: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Parameters {
    pub(crate) vps: Vec<Vec<u8>>,
    pub(crate) sps: Vec<Vec<u8>>,
    pub(crate) pps: Vec<Vec<u8>>,
}

impl Parameters {
    pub(crate) fn bit_depth(&self) -> Result<u8> {
        let sps = self
            .description()?
            .context("HEVC configuration has no SPS")?;
        Ok(sps.depth + 8)
    }

    fn description(&self) -> Result<Option<Sps>> {
        let mut count = 0;
        let mut size = 23 + 3 * 3; // hvcC header and three array headers.
        let mut description = None;
        for (kind, array) in [(32, &self.vps), (33, &self.sps), (34, &self.pps)] {
            count += array.len();
            if count > MAX_NALS {
                bail!("HEVC configuration has too many parameter sets");
            }
            for nal in array {
                if nal.len() > u16::MAX as usize || header(nal)? != kind {
                    bail!("invalid HEVC parameter set type or length");
                }
                size += 2 + nal.len();
                if size > MAX_PACKET {
                    bail!("HEVC configuration is too large");
                }
                let mut bits = Bits::new(nal)?;
                match kind {
                    32 => {
                        bits.read(4)?; // vps_video_parameter_set_id
                        let base = bits.read(2)?;
                        let layers = bits.read(6)?;
                        let temporal = bits.read(3)?;
                        let nested = bits.read(1)?;
                        if base != 3 || layers != 0 || bits.read(16)? != 0xffff {
                            bail!("HEVC VPS must describe one internal base layer");
                        }
                        profile(&mut bits, temporal, nested)?;
                    }
                    33 => {
                        let sps = Sps::read(&mut bits)?;
                        if description.as_ref().is_some_and(|first| *first != sps) {
                            bail!("HEVC SPS metadata differs within one configuration");
                        }
                        description = Some(sps);
                    }
                    34 => {
                        if bits.ue()? > 63 || bits.ue()? > 15 {
                            bail!("invalid HEVC PPS or SPS id");
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
        Ok(description)
    }
}

pub(crate) fn parameters(nals: &[&[u8]]) -> Result<Parameters> {
    if nals.len() > MAX_NALS {
        bail!("HEVC access unit has too many NAL units");
    }
    let mut sets = Parameters::default();
    let mut size = 0usize;
    for nal in nals {
        size = size.checked_add(nal.len()).context("HEVC size overflow")?;
        if size > MAX_PACKET {
            bail!("HEVC parameter source is too large");
        }
        let array = match header(nal)? {
            32 => &mut sets.vps,
            33 => &mut sets.sps,
            34 => &mut sets.pps,
            _ => continue,
        };
        if nal.len() > u16::MAX as usize {
            bail!("HEVC parameter set is too large");
        }
        array.push(nal.to_vec());
    }
    sets.description()?;
    Ok(sets)
}

pub(crate) fn config(sets: &Parameters) -> Result<Vec<u8>> {
    let sps = sets
        .description()?
        .context("HEVC configuration has no SPS")?;
    if sets.vps.is_empty() || sets.pps.is_empty() {
        bail!("HEVC configuration needs VPS, SPS and PPS");
    }
    let mut data = vec![1];
    data.extend_from_slice(&sps.profile);
    data.extend_from_slice(&[
        0xf0,
        0,    // Unknown minimum spatial segmentation.
        0xfc, // Unknown parallelism type.
        0xfc | sps.chroma,
        0xf8 | sps.depth,
        0xf8 | sps.depth,
        0,
        0, // Unspecified average/constant frame rate.
        (sps.temporal + 1) << 3 | sps.nested << 2 | 3,
        3, // VPS, SPS, PPS arrays; four-byte sample NAL lengths above.
    ]);
    for (kind, array) in [(32, &sets.vps), (33, &sets.sps), (34, &sets.pps)] {
        data.push(0x80 | kind); // hvc1 requires array_completeness.
        data.extend_from_slice(&(array.len() as u16).to_be_bytes());
        for nal in array {
            data.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            data.extend_from_slice(nal);
        }
    }
    Ok(data)
}

pub(crate) fn packet(
    source: &[u8],
    pts: Time,
    sets: &mut Parameters,
) -> Result<Option<EncodedVideo>> {
    if source.is_empty() || source.len() > MAX_PACKET {
        bail!("invalid HEVC packet size {}", source.len());
    }
    let nals = annex_b(source).context("invalid HEVC Annex B packet")?;
    let mut next = parameters(&nals)?;
    let changed = !next.vps.is_empty() || !next.sps.is_empty() || !next.pps.is_empty();
    if changed {
        if next.vps.is_empty() {
            next.vps = sets.vps.clone();
        }
        if next.sps.is_empty() {
            next.sps = sets.sps.clone();
        }
        if next.pps.is_empty() {
            next.pps = sets.pps.clone();
        }
        let description = next.description()?;
        if !sets.sps.is_empty() && sets.description()? != description {
            bail!("HEVC SPS metadata changed; open a new encoder configuration");
        }
        if !sets.vps.is_empty() && !sets.sps.is_empty() && !sets.pps.is_empty() && next != *sets {
            bail!("HEVC parameter sets changed; open a new hvc1 track");
        }
    }
    let mut data = Vec::new();
    data.try_reserve(source.len().saturating_add(nals.len() * 4))?;
    let mut picture = false;
    let mut keyframe = false;
    for nal in nals {
        let kind = (nal[0] >> 1) & 63;
        if (32..=34).contains(&kind) {
            continue;
        }
        picture |= kind <= 31;
        keyframe |= matches!(kind, 19 | 20); // IDR only, not open-GOP CRA.
        if data.len() + 4 + nal.len() > MAX_PACKET {
            bail!("length-prefixed HEVC packet is too large");
        }
        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        data.extend_from_slice(nal);
    }
    // Do not poison cached headers if any validation or packet conversion failed.
    if changed {
        *sets = next;
    }
    if !picture {
        return Ok(None);
    }
    Ok(Some(EncodedVideo {
        data,
        pts,
        dts: pts,
        keyframe,
        parameter_sets: None, // This field is H.264-only; use config() for HEVC.
    }))
}

fn header(nal: &[u8]) -> Result<u8> {
    if nal.len() < 3 || nal[0] & 0x80 != 0 || nal[1] & 7 == 0 {
        bail!("invalid HEVC NAL header or empty payload");
    }
    if nal[0] & 1 != 0 || nal[1] & 0xf8 != 0 {
        bail!("HEVC supports only layer zero");
    }
    let kind = (nal[0] >> 1) & 63;
    if matches!(kind, 16..=23 | 32 | 33 | 36 | 37) && nal[1] & 7 != 1 {
        bail!("HEVC IRAP/VPS/SPS/EOS/EOB must have temporal id zero");
    }
    Ok(kind)
}

#[derive(PartialEq, Eq)]
struct Sps {
    profile: [u8; 12],
    temporal: u8,
    nested: u8,
    chroma: u8,
    depth: u8,
    dimensions: [u32; 6],
}

impl Sps {
    fn read(bits: &mut Bits) -> Result<Self> {
        bits.read(4)?; // sps_video_parameter_set_id
        let temporal = bits.read(3)?;
        let nested = bits.read(1)?;
        let profile = profile(bits, temporal, nested)?;
        if bits.ue()? > 15 {
            bail!("invalid HEVC SPS id");
        }
        let chroma = bits.ue()?;
        if chroma != 1 {
            bail!("HEVC supports only 4:2:0 chroma");
        }
        let mut dimensions = [bits.ue()?, bits.ue()?, 0, 0, 0, 0];
        if bits.read(1)? != 0 {
            for offset in &mut dimensions[2..] {
                *offset = bits.ue()?;
            }
        }
        let [width, height, left, right, top, bottom] = dimensions;
        if 2 * (u64::from(left) + u64::from(right)) >= u64::from(width)
            || 2 * (u64::from(top) + u64::from(bottom)) >= u64::from(height)
        {
            bail!("invalid HEVC SPS dimensions or conformance window");
        }
        let depth = bits.ue()?;
        let chroma_depth = bits.ue()?;
        if !matches!(depth, 0 | 2) || depth != chroma_depth || (profile[0] & 31 == 1 && depth != 0)
        {
            bail!("HEVC requires 8-bit Main or 8/10-bit Main10");
        }
        if bits.ue()? > 12 {
            bail!("invalid HEVC picture-order-count width");
        }
        let first = if bits.read(1)? != 0 { 0 } else { temporal };
        for _ in first..=temporal {
            let buffering = bits.ue()?;
            let reorder = bits.ue()?;
            bits.ue()?; // sps_max_latency_increase_plus1
            if buffering > 15 || reorder != 0 {
                bail!("HEVC requires bounded buffering without reordered pictures");
            }
        }
        Ok(Self {
            profile,
            temporal: temporal as u8,
            nested: nested as u8,
            chroma: chroma as u8,
            depth: depth as u8,
            dimensions,
        })
    }
}

fn profile(bits: &mut Bits, temporal: u32, nested: u32) -> Result<[u8; 12]> {
    if temporal > 6 || (temporal == 0 && nested != 1) {
        bail!("invalid HEVC temporal-layer count or nesting");
    }
    let mut profile = [0; 12];
    for byte in &mut profile {
        *byte = bits.read(8)? as u8;
    }
    if profile[0] & 0xc0 != 0 || !matches!(profile[0] & 31, 1 | 2) {
        bail!("HEVC supports only Main and Main10 profiles");
    }
    if profile[5] & 0x40 != 0 {
        bail!("interlaced HEVC is unsupported");
    }
    let mut flags = [0; 6];
    for flag in &mut flags[..temporal as usize] {
        *flag = bits.read(2)?;
    }
    if temporal != 0 {
        for _ in temporal..8 {
            if bits.read(2)? != 0 {
                bail!("invalid HEVC reserved profile bits");
            }
        }
    }
    for flag in &flags[..temporal as usize] {
        if flag & 2 != 0 {
            for _ in 0..11 {
                bits.read(8)?; // sub_layer_profile_*: 88 bits.
            }
        }
        if flag & 1 != 0 {
            bits.read(8)?; // sub_layer_level_idc
        }
    }
    Ok(profile)
}

struct Bits {
    data: Vec<u8>,
    at: usize,
    end: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Result<Self> {
        // Called only for already size-bounded, header-checked parameter sets.
        let mut data = Vec::with_capacity(nal.len() - 2);
        let mut zeros = 0;
        for (i, &byte) in nal[2..].iter().enumerate() {
            if zeros == 2 {
                if byte == 3 {
                    if nal.get(i + 3).is_none_or(|next| *next > 3) {
                        bail!("invalid HEVC emulation-prevention byte");
                    }
                    zeros = 0;
                    continue;
                }
                if byte < 3 {
                    bail!("unescaped HEVC start-code sequence");
                }
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            data.push(byte);
        }
        let last = data
            .last()
            .filter(|byte| **byte != 0)
            .context("HEVC parameter set has no RBSP stop bit")?;
        // The stop bit is not syntax: a truncated ue(v) must not consume it.
        let end = data.len() * 8 - last.trailing_zeros() as usize - 1;
        Ok(Self { data, at: 0, end })
    }

    fn read(&mut self, count: usize) -> Result<u32> {
        if count > 32 || self.at + count > self.end {
            bail!("truncated HEVC parameter set");
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from((self.data[self.at / 8] >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        Ok(value)
    }

    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                bail!("HEVC Exp-Golomb value is too large");
            }
        }
        Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
}
