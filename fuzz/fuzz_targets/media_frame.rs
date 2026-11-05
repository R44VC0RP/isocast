#![no_main]
use isocast_media_protocol::{Header, read_header};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut input = data;
    while let Ok(header) = read_header(&mut input) {
        let n = match header {
            Header::Video { w, h, bytes, .. } => {
                assert!((1..=8192).contains(&w) && (1..=8192).contains(&h));
                assert_eq!(bytes, (w * h * 4) as usize);
                bytes
            }
            Header::Audio { bytes, .. } => {
                assert!(bytes.is_multiple_of(8));
                bytes
            }
            Header::Path(n) | Header::Check(n) | Header::Info(n) | Header::Fail(n) => n,
            _ => 0,
        };
        let Some(rest) = input.get(n..) else { break };
        input = rest;
    }
});
