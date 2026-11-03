//! Diagnostic receiver, not a Windows camera registration. Run with the
//! logical pipe path from `isocast vcam status --json` (destination),
//! seconds, optional sleep milliseconds, optional reference NV12 picture.
//! The reference scene's upper-left quadrant must be static; the remaining
//! picture can animate. Convert a program grab to packed Rec.709 NV12 first.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use isocast_vcam_wire::{HEADER_BYTES, Header};
    use std::io::Read;
    use std::time::{Duration, Instant};

    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .context("vcam-reader PATH SECONDS [SLEEP_MS [REFERENCE.nv12]]")?;
    let seconds: u64 = args.next().context("give seconds")?.parse()?;
    let delay: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let reference = args.next().map(std::fs::read).transpose()?;
    ensure!(args.next().is_none() && seconds > 0, "invalid arguments");
    let mut pipe = isocast_ipc::LocalStream::connect(path)?;
    pipe.set_read_timeout(Some(Duration::from_secs(5)))?;
    let start = Instant::now();
    let mut seq = 0;
    let (mut frames, mut skipped) = (0, 0);
    let mut previous = Vec::new();
    let mut bytes = Vec::new();
    let mut last = None;
    while start.elapsed() < Duration::from_secs(seconds) {
        let mut h = [0; HEADER_BYTES];
        pipe.read_exact(&mut h)?;
        let header = Header::decode(&h, last)?;
        let (w, hgt) = (header.mode.width() as usize, header.mode.height() as usize);
        let pixels = w * hgt;
        let (n, source, delivery, repeat) = (
            header.sequence,
            header.source_ns,
            header.delivery_ns,
            u8::from(header.repeated),
        );
        bytes.resize(header.mode.nv12_len(), 0);
        pipe.read_exact(&mut bytes)?;
        if let Some(r) = &reference {
            ensure!(r.len() == bytes.len(), "reference size differs");
            for (offset, rows) in [(0, hgt / 2), (pixels, hgt / 4)] {
                let mut error = 0usize;
                for y in 0..rows {
                    let row = offset + y * w;
                    error += (row..row + w / 2)
                        .map(|i| usize::from(bytes[i].abs_diff(r[i])))
                        .sum::<usize>();
                }
                ensure!(
                    error <= rows * (w / 2) * 6,
                    "picture differs from reference at plane {offset}"
                );
            }
        }
        let changed = seq != 0 && bytes != previous;
        if repeat == 1 && seq != 0 && n == seq + 1 {
            ensure!(!changed, "repeat changed accepted pixels or PTS");
        }
        if seq != 0 {
            skipped += n - seq - 1;
        }
        seq = n;
        last = Some(header);
        frames += 1;
        println!("{seq},{delivery},{source},{repeat},{}", u8::from(changed));
        std::mem::swap(&mut previous, &mut bytes);
        std::thread::sleep(Duration::from_millis(delay));
    }
    eprintln!(
        "{frames} frames, {skipped} skipped, {:?}",
        last.map(|h| h.mode)
    );
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vcam-reader needs Windows");
}
