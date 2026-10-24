//! The common H.264 packet boundary of NVENC and Media Foundation.

use anyhow::{Context, Result, bail};
use isocast_engine::{EncodedVideo, ParameterSets, Time};
use isocast_protocol::api::H264Profile;

pub(crate) const MAX_PACKET: usize = 24 << 20;
const MAX_NALS: usize = 1024;

pub(crate) fn config(sets: &ParameterSets) -> Result<Vec<u8>> {
    let first = sets
        .sps
        .first()
        .filter(|sps| sps.len() >= 4)
        .context("H.264 SPS is too short")?;
    if sets.sps.len() > 31 || sets.pps.len() > 255 || sets.pps.is_empty() {
        bail!("invalid H.264 parameter set count");
    }
    let mut data = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xff,
        0xe0 | sets.sps.len() as u8,
    ];
    for sps in &sets.sps {
        write_set(&mut data, sps)?;
    }
    data.push(sets.pps.len() as u8);
    for pps in &sets.pps {
        write_set(&mut data, pps)?;
    }
    if first[1] == 100 {
        data.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    Ok(data)
}

pub(crate) fn packet(
    source: &[u8],
    pts: Time,
    sets: &mut ParameterSets,
) -> Result<Option<EncodedVideo>> {
    if source.is_empty() || source.len() > MAX_PACKET {
        bail!("invalid H.264 packet size {}", source.len());
    }
    let nals = annex_b(source)?;
    let next = parameters(&nals);
    if !next.sps.is_empty() {
        sets.sps = next.sps;
    }
    if !next.pps.is_empty() {
        sets.pps = next.pps;
    }
    let mut data = Vec::new();
    data.try_reserve(source.len().saturating_add(nals.len() * 4))?;
    let mut picture = false;
    let mut keyframe = false;
    for nal in nals {
        let kind = nal[0] & 31;
        if kind == 7 || kind == 8 {
            continue;
        }
        picture |= (1..=5).contains(&kind);
        keyframe |= kind == 5;
        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        data.extend_from_slice(nal);
    }
    if !picture {
        return Ok(None);
    }
    Ok(Some(EncodedVideo {
        data,
        pts,
        dts: pts,
        keyframe,
        parameter_sets: keyframe.then(|| sets.clone()),
    }))
}

pub(crate) fn annex_b(data: &[u8]) -> Result<Vec<&[u8]>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let prefix = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if starts.len() == MAX_NALS {
            bail!("H.264 access unit has too many NAL units");
        }
        starts.push((i, i + prefix));
        i += prefix;
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (index, &(begin, start)) in starts.iter().enumerate() {
        if index == 0 && data[..begin].iter().any(|byte| *byte != 0) {
            bail!("H.264 packet has bytes before its first start code");
        }
        let mut end = starts.get(index + 1).map_or(data.len(), |next| next.0);
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if start == end {
            bail!("H.264 packet contains an empty NAL unit");
        }
        nals.push(&data[start..end]);
    }
    if nals.is_empty() {
        bail!("H.264 packet has no Annex B start codes");
    }
    Ok(nals)
}

pub(crate) fn parameters(nals: &[&[u8]]) -> ParameterSets {
    ParameterSets {
        sps: nals
            .iter()
            .filter(|nal| nal[0] & 31 == 7)
            .map(|nal| nal.to_vec())
            .collect(),
        pps: nals
            .iter()
            .filter(|nal| nal[0] & 31 == 8)
            .map(|nal| nal.to_vec())
            .collect(),
    }
}

/// Fail unless the parameter sets really say the requested profile. High
/// keeps whatever the encoder made (as before); Constrained Baseline needs
/// every SPS at profile_idc 66 with constraint_set1_flag and every PPS on
/// CAVLC, never a stream merely asked for as baseline (or quietly High).
pub(crate) fn check_profile(sets: &ParameterSets, profile: H264Profile) -> Result<()> {
    if profile == H264Profile::High {
        return Ok(());
    }
    for set in &sets.sps {
        match set.get(1..3) {
            Some(&[66, flags]) if flags & 0x40 != 0 => {}
            Some(&[idc, flags]) => bail!(
                "video.profile: constrained_baseline was requested, but the encoder made \
                 profile_idc {idc} with constraint flags {flags:#04x} (needs 66 with \
                 constraint_set1); this encoder cannot make it, use high"
            ),
            _ => bail!("H.264 SPS is too short"),
        }
    }
    for set in &sets.pps {
        if pps_cabac(set).context("H.264 PPS is too short")? {
            bail!(
                "video.profile: constrained_baseline was requested, but the encoder made a \
                 CABAC picture parameter set; this encoder cannot make it, use high"
            );
        }
    }
    Ok(())
}

/// A PPS's entropy_coding_mode_flag: after pic_parameter_set_id and
/// seq_parameter_set_id (both ue(v)), with emulation prevention removed.
fn pps_cabac(pps: &[u8]) -> Option<bool> {
    let mut bits = Vec::with_capacity(8);
    let mut zeros = 0;
    for &byte in pps.get(1..)?.iter().take(16) {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        bits.push(byte);
    }
    let bit = |n: usize| bits.get(n / 8).map(|byte| (byte >> (7 - n % 8)) & 1);
    let mut at = 0;
    for _ in 0..2 {
        let mut leading = 0;
        while bit(at)? == 0 {
            leading += 1;
            at += 1;
            if leading > 31 {
                return None;
            }
        }
        at += 1 + leading;
    }
    Some(bit(at)? == 1)
}

fn write_set(data: &mut Vec<u8>, set: &[u8]) -> Result<()> {
    let len = u16::try_from(set.len()).context("H.264 parameter set is too large")?;
    data.extend_from_slice(&len.to_be_bytes());
    data.extend_from_slice(set);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrained_baseline_needs_profile_66_with_constraint_set1() {
        // NAL header, profile_idc, constraint flags, level_idc.
        let constrained = vec![0x67, 66, 0xc0, 31];
        let plain_baseline = vec![0x67, 66, 0x80, 31];
        let high = vec![0x67, 100, 0x00, 31];
        let sps = |sps: &[&Vec<u8>]| ParameterSets {
            sps: sps.iter().map(|set| (*set).clone()).collect(),
            pps: Vec::new(),
        };
        let profile = H264Profile::ConstrainedBaseline;
        check_profile(&sps(&[&constrained]), profile).unwrap();
        let error = check_profile(&sps(&[&plain_baseline]), profile).unwrap_err();
        assert!(error.to_string().contains("video.profile"), "{error}");
        assert!(check_profile(&sps(&[&high]), profile).is_err());
        assert!(check_profile(&sps(&[&constrained, &high]), profile).is_err());
        assert!(check_profile(&sps(&[&vec![0x67, 66]]), profile).is_err());
        // High keeps whatever the encoder made, as before.
        check_profile(&sps(&[&high, &plain_baseline]), H264Profile::High).unwrap();
    }

    #[test]
    fn constrained_baseline_needs_cavlc_pictures() {
        let with_pps = |pps: Vec<u8>| ParameterSets {
            sps: vec![vec![0x67, 66, 0xc0, 31]],
            pps: vec![pps],
        };
        let profile = H264Profile::ConstrainedBaseline;
        // pps_id 0, sps_id 0 ("1", "1"), then entropy_coding_mode_flag.
        check_profile(&with_pps(vec![0x68, 0b1100_0000]), profile).unwrap();
        let error = check_profile(&with_pps(vec![0x68, 0b1110_0000]), profile).unwrap_err();
        assert!(error.to_string().contains("CABAC"), "{error}");
        // pps_id 1 ("010"), sps_id 2 ("011"), CABAC: 0100 1110 0000.
        assert_eq!(pps_cabac(&[0x68, 0b0100_1110, 0]), Some(true));
        assert_eq!(pps_cabac(&[0x68, 0b0100_1100, 0]), Some(false));
        assert!(check_profile(&with_pps(vec![0x68]), profile).is_err());
        check_profile(&with_pps(vec![0x68, 0b1110_0000]), H264Profile::High).unwrap();
    }

    #[test]
    fn constrained_baseline_avcc_has_no_high_extension() {
        let sets = ParameterSets {
            sps: vec![vec![0x67, 66, 0xc0, 31, 0xaa]],
            pps: vec![vec![0x68, 0xce]],
        };
        let config = config(&sets).unwrap();
        assert_eq!(&config[..4], &[1, 66, 0xc0, 31]);
        assert!(config.ends_with(&[1, 0, 2, 0x68, 0xce]));
    }
}
