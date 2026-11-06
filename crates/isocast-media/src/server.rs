use std::{
    io::{self, Read},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use crate::decoder;
use anyhow::{Result, bail, ensure};
use isocast_media_protocol::{Header, MediaPart, MediaReader, read_header, write_record};

pub(super) fn serve(out: &mut std::fs::File, operation: &str) -> Result<()> {
    let mut input = io::stdin().lock();
    decoder::init()?;
    write_record(out, b"INIT", &[])?;
    if operation == "check" {
        let json = serde_json::to_vec(&decoder::decoders_check())?;
        return Ok(write_record(out, b"CHCK", &[&json])?);
    }
    let Header::Path(len) = read_header(&mut input)? else {
        bail!("expected media path")
    };
    let mut bytes = vec![0; len];
    input.read_exact(&mut bytes)?;
    let path = Path::new(std::ffi::OsStr::from_bytes(&bytes));
    let part = match operation {
        "info" => {
            let info = decoder::media_info(path)?;
            let json = serde_json::to_vec(&(
                info.video,
                info.audio,
                u64::try_from(info.duration.as_nanos())?,
            ))?;
            return Ok(write_record(out, b"INFO", &[&json])?);
        }
        "video" => MediaPart::Video,
        "audio" => MediaPart::Audio,
        _ => bail!("unknown media operation"),
    };
    let mut reader = decoder::open_media(path, part)?;
    let mut test_stdout = std::env::var_os("ISOCAST_TEST_MEDIA_STDOUT").is_some();
    write_record(out, b"REDY", &[])?;
    loop {
        ensure!(read_header(&mut input)? == Header::Next, "expected NEXT");
        match part {
            MediaPart::Video => {
                if let Some(((w, h, pixels), at)) = reader.next_video()? {
                    if std::mem::take(&mut test_stdout) {
                        gstreamer::glib::g_print!("isocast-media: decoded sample stdout probe\n");
                    }
                    write_record(
                        out,
                        b"VBGR",
                        &[
                            &u64::try_from(at.as_nanos())?.to_le_bytes(),
                            &w.to_le_bytes(),
                            &h.to_le_bytes(),
                            &pixels,
                        ],
                    )?;
                    continue;
                }
            }
            MediaPart::Audio => {
                if let Some((samples, at)) = reader.next_audio()? {
                    let pcm: Vec<_> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                    write_record(
                        out,
                        b"AF32",
                        &[&u64::try_from(at.as_nanos())?.to_le_bytes(), &pcm],
                    )?;
                    continue;
                }
            }
        }
        return Ok(write_record(out, b"END!", &[])?);
    }
}
