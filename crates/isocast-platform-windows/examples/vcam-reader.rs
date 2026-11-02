//! Diagnostic receiver, not a Windows camera registration. Run with the
//! logical pipe path from `isocast vcam status --json` (destination),
//! seconds, optional sleep milliseconds, optional reference NV12 picture.
//! The reference scene's upper-left quadrant must be static; the remaining
//! picture can animate. Convert a program grab to packed Rec.709 NV12 first.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
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
    let (mut seq, mut stamp, mut pts) = (0, 0, 0);
    let (mut frames, mut skipped) = (0, 0);
    let mut previous = Vec::new();
    let mut bytes = Vec::new();
    let mut mode = None;
    while start.elapsed() < Duration::from_secs(seconds) {
        let mut h = [0; 48];
        pipe.read_exact(&mut h)?;
        ensure!(&h[..8] == b"ICVCNV01", "unknown camera protocol");
        let field = |i| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
        let (w, hgt, fps, repeat) = (field(8) as usize, field(12) as usize, field(16), field(20));
        let n = u64::from_le_bytes(h[24..32].try_into().unwrap());
        let source = i64::from_le_bytes(h[32..40].try_into().unwrap());
        let delivery = i64::from_le_bytes(h[40..48].try_into().unwrap());
        let pixels = w.checked_mul(hgt).context("frame area overflow")?;
        ensure!(
            w >= 2
                && hgt >= 2
                && w.is_multiple_of(2)
                && hgt.is_multiple_of(2)
                && pixels <= 3840 * 2160
                && (1..=240).contains(&fps)
                && repeat <= 1,
            "invalid camera header"
        );
        ensure!(
            *mode.get_or_insert((w, hgt, fps)) == (w, hgt, fps),
            "format changed without reconnect"
        );
        ensure!(
            n > seq && delivery > stamp && source >= pts && source <= delivery,
            "non-monotonic timestamp or sequence: {n} {source} {delivery}"
        );
        bytes.resize(pixels * 3 / 2, 0);
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
            ensure!(
                source == pts && !changed,
                "repeat changed accepted pixels or PTS"
            );
        }
        if seq != 0 {
            skipped += n - seq - 1;
        }
        (seq, stamp, pts) = (n, delivery, source);
        frames += 1;
        println!("{seq},{stamp},{pts},{repeat},{}", u8::from(changed));
        std::mem::swap(&mut previous, &mut bytes);
        std::thread::sleep(Duration::from_millis(delay));
    }
    eprintln!("{frames} frames, {skipped} skipped, {mode:?}");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("vcam-reader needs Windows");
}
