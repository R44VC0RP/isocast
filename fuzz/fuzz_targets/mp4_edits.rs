//! A QuickTime/MP4 file's edit lists, as Windows' media reader reads them
//! from any media file a source names: no panics, no runaway reads, read
//! as a movie header and as a whole file.
#![no_main]

use std::io::Cursor;

use isocast_protocol::mp4_edits::{parse_moov, read_from};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let _ = parse_moov(bytes);
    let _ = read_from(&mut Cursor::new(bytes));
});
