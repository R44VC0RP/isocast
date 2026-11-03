#![no_main]

use isocast_vcam_wire::{HEADER_BYTES, Header};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let mut previous = None;
    for raw in bytes.as_chunks::<HEADER_BYTES>().0 {
        match Header::decode(raw, previous) {
            Ok(h) => {
                assert_eq!(h.encode(), *raw);
                assert!(h.mode.nv12_len() <= 3840 * 2160 * 3 / 2);
                previous = Some(h);
            }
            Err(_) => break,
        }
    }
});
