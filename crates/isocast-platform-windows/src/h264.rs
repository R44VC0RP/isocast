//! The common H.264 packet boundary of NVENC and Media Foundation.

use anyhow::{Context, Result, bail};
use isocast_engine::{EncodedVideo, ParameterSets, Time};

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

fn write_set(data: &mut Vec<u8>, set: &[u8]) -> Result<()> {
    let len = u16::try_from(set.len()).context("H.264 parameter set is too large")?;
    data.extend_from_slice(&len.to_be_bytes());
    data.extend_from_slice(set);
    Ok(())
}
