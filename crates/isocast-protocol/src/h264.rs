//! H.264 framing a client reads from an engine's video (untrusted input):
//! the `avcC` record of the `frames` stream's config, AVCC access units
//! (each NAL unit after its 4-byte length) and Annex B (start codes, as
//! WebRTC receivers hand H.264 over). Parsing only; no decoding. Nothing
//! here panics or allocates more than its input on malformed bytes (the
//! fuzz target `fuzz/fuzz_targets/h264.rs` and [`tests`] check it).

/// An H.264 stream's parameter sets, as NAL units without start codes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

/// Why an `avcC` record couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AvccError {
    #[error("a short avcC record ({0} bytes)")]
    Short(usize),
    #[error("an avcC record without an SPS or a PPS")]
    Incomplete,
}

impl ParameterSets {
    /// The sets in an `AVCDecoderConfigurationRecord` (`avcC`).
    pub fn from_avcc(avcc: &[u8]) -> Result<Self, AvccError> {
        let short = || AvccError::Short(avcc.len());
        let mut sets = Self::default();
        let mut at = 5;
        for (mask, out) in [(0x1f, &mut sets.sps), (0xff, &mut sets.pps)] {
            let count = usize::from(*avcc.get(at).ok_or_else(short)? & mask);
            at += 1;
            for _ in 0..count {
                let len = avcc.get(at..at + 2).ok_or_else(short)?;
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                out.push(avcc.get(at + 2..at + 2 + len).ok_or_else(short)?.to_vec());
                at += 2 + len;
            }
        }
        if sets.sps.is_empty() || sets.pps.is_empty() {
            return Err(AvccError::Incomplete);
        }
        Ok(sets)
    }
}

/// The NAL units of an AVCC access unit; stops at the first unit whose
/// length runs past the end.
pub fn nal_units(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        let len = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let nal = data.get(4..4usize.checked_add(len)?)?;
        data = &data[4 + len..];
        Some(nal)
    })
}

/// The NAL units between Annex B start codes (3 or 4 bytes), without the
/// zero bytes before the next start code.
pub fn annex_b_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= data.len() {
        if data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 1 {
            starts.push(at + 3);
            at += 3;
        } else {
            at += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|next| {
            let mut end = next - 3;
            while end > 0 && data[end - 1] == 0 {
                end -= 1;
            }
            end
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(start, end)| end > start)
        .map(move |(start, end)| &data[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_parameter_sets_and_nal_units() {
        // version, profile, compat, level, 0xff (4-byte lengths),
        // 1 SPS of 3 bytes, 1 PPS of 2 bytes.
        let avcc = [
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9,
        ];
        let sets = ParameterSets::from_avcc(&avcc).unwrap();
        assert_eq!(sets.sps, vec![vec![0x67, 1, 2]]);
        assert_eq!(sets.pps, vec![vec![0x68, 9]]);
        assert_eq!(
            ParameterSets::from_avcc(&avcc[..9]),
            Err(AvccError::Short(9))
        );

        let unit = [0, 0, 0, 2, 0x65, 7, 0, 0, 0, 1, 0x41];
        let nals: Vec<&[u8]> = nal_units(&unit).collect();
        assert_eq!(nals, vec![&[0x65, 7][..], &[0x41][..]]);
        // A length past the end, or near u32::MAX, stops the units.
        assert_eq!(nal_units(&[0, 0, 0, 9, 1]).count(), 0);
        assert_eq!(nal_units(&[0xff, 0xff, 0xff, 0xff, 1]).count(), 0);
    }

    #[test]
    fn annex_b_units_between_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 7, 0];
        let units: Vec<&[u8]> = annex_b_units(&data).collect();
        assert_eq!(units, vec![&[0x67, 1][..], &[0x68][..], &[0x65, 7, 0][..]]);
        assert_eq!(annex_b_units(&[0, 0, 1]).count(), 0);
        assert_eq!(annex_b_units(&[]).count(), 0);
    }

    /// The fuzz target's checks on many pseudo-random and mutated inputs,
    /// so they run in every `cargo test` (cargo-fuzz explores further).
    #[test]
    fn malformed_input_never_panics() {
        let seeds: [&[u8]; 3] = [
            &[
                1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 9,
            ],
            &[0, 0, 0, 2, 0x65, 7, 0, 0, 0, 1, 0x41],
            &[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 7],
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..20_000 {
            let mut bytes = seeds[round % seeds.len()].to_vec();
            let len = (next() % 48) as usize;
            if round % 4 == 0 {
                bytes = (0..len).map(|_| next() as u8).collect();
            } else {
                for _ in 0..1 + next() % 4 {
                    if bytes.is_empty() {
                        break;
                    }
                    let at = (next() as usize) % bytes.len();
                    match next() % 3 {
                        0 => bytes[at] = next() as u8,
                        1 => bytes.truncate(at),
                        _ => bytes.insert(at, next() as u8),
                    }
                }
            }
            check(&bytes);
        }
    }

    /// What the fuzz target asserts for any input.
    pub fn check(bytes: &[u8]) {
        if let Ok(sets) = ParameterSets::from_avcc(bytes) {
            let total: usize = sets.sps.iter().chain(&sets.pps).map(Vec::len).sum();
            assert!(total <= bytes.len());
        }
        let mut seen = 0;
        for nal in nal_units(bytes) {
            seen += 4 + nal.len();
        }
        assert!(seen <= bytes.len());
        assert!(annex_b_units(bytes).map(<[u8]>::len).sum::<usize>() <= bytes.len());
    }
}
