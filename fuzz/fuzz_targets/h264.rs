//! The H.264 framing a desktop client reads from an engine's video
//! (`frames` records, their avcC config, AVCC and Annex B access units):
//! no panics, and nothing parsed is longer than what came in.
#![no_main]

use isocast_protocol::api::frames::Record;
use isocast_protocol::h264::{ParameterSets, annex_b_units, nal_units};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(sets) = ParameterSets::from_avcc(bytes) {
        let total: usize = sets.sps.iter().chain(&sets.pps).map(Vec::len).sum();
        assert!(total <= bytes.len());
    }
    let seen: usize = nal_units(bytes).map(|nal| 4 + nal.len()).sum();
    assert!(seen <= bytes.len());
    assert!(annex_b_units(bytes).map(<[u8]>::len).sum::<usize>() <= bytes.len());
    // A stream of `frames` records, as the client reads them.
    let mut reader = bytes;
    while let Ok(Some(record)) = Record::read(&mut reader) {
        assert!(record.payload.len() <= bytes.len());
    }
});
