//! Explicit non-installing diagnostic receiver. Uses the production output
//! worker and native sample construction, transferring pixels by Mach rights.
//! An acknowledgement holds the CVPixelBuffer until this reader is done;
//! stalls time out instead of retaining recording frames indefinitely.
use super::{PixelBuffer, isocast_camera_validate};
use anyhow::{Context, Result, bail};
use isocast_engine::{Time, record::CameraSink};
use isocast_surfaces::{Exporter, Surface, Token};
use objc2_core_video::CVPixelBufferGetIOSurface;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

pub(super) struct Probe {
    socket: UnixStream,
    fps: u32,
    exporter: &'static Exporter,
}

impl Probe {
    pub(super) fn connect(path: &Path, fps: u32) -> Result<Self> {
        let mut socket = UnixStream::connect(path)
            .context("ISOCAST_VCAM_PROBE: start the probe reader first; after SIGKILL use a fresh socket path or remove only your own stale socket")?;
        socket.set_write_timeout(Some(Duration::from_millis(200)))?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        let exporter = Exporter::shared()?;
        writeln!(socket, "{}", exporter.service())?;
        Ok(Self {
            socket,
            fps,
            exporter,
        })
    }
}

impl CameraSink<PixelBuffer> for Probe {
    fn transport(&self) -> &'static str {
        "diagnostic-iosurface"
    }
    fn send(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        let native = unsafe {
            isocast_camera_validate(
                (&*frame.0 as *const objc2_core_video::CVPixelBuffer).cast(),
                pts.as_nanos(),
                self.fps,
            )
        };
        if native != 0 {
            bail!("camera sample failed IOSurface/Rec.709/timestamp validation ({native})");
        }
        let surface =
            CVPixelBufferGetIOSurface(Some(&frame.0)).context("camera frame has no IOSurface")?;
        let surface = Surface::from_io_surface(surface);
        let id = surface.id();
        let offered = self.exporter.offer(vec![surface.clone()]);
        let mut header = [0u8; 48];
        header[0..16].copy_from_slice(&offered.0);
        for (at, value) in [
            (16, id),
            (20, surface.width()),
            (24, surface.height()),
            (28, self.fps),
            (40, 0x42475241),
            (44, 709),
        ] {
            header[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        header[32..40].copy_from_slice(&pts.as_nanos().to_le_bytes());
        let result = (|| {
            self.socket.write_all(&header)?;
            let mut ack = [0u8; 1];
            self.socket.read_exact(&mut ack)?;
            if ack != [1] {
                bail!("camera probe refused a frame");
            }
            Ok(true)
        })();
        self.exporter.withdraw(offered);
        result
    }
}

/// Listen once, check IOSurface pixels, sample timing and colour metadata,
/// optionally save the first frame as PPM, and print a JSON summary at EOF.
pub fn read(path: &Path, picture: Option<&Path>) -> Result<()> {
    // Never unlink a pre-existing path: it may belong to another reader.
    // This standalone reader has not started any threads yet.
    let mask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(path);
    unsafe {
        libc::umask(mask);
    }
    let listener = listener?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    struct Remove<'a>(&'a Path);
    impl Drop for Remove<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _remove = Remove(path);
    let (mut socket, _) = listener.accept()?;
    socket.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(socket.try_clone()?);
    let mut service = String::new();
    reader.read_line(&mut service)?;
    let mut surfaces = HashMap::new();
    let mut times = Vec::new();
    let mut latency = Vec::new();
    let mut size = (0, 0, 0);
    let mut colors = Vec::new();
    let mut surface_ids = std::collections::HashSet::new();
    let mut hashes = std::collections::HashSet::new();
    loop {
        let mut header = [0u8; 48];
        match reader.read(&mut header[..1])? {
            0 => break,
            _ => reader.read_exact(&mut header[1..])?,
        }
        let field = |at| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
        let id = field(16);
        surface_ids.insert(id);
        let token = Token(header[..16].try_into().unwrap());
        if token.0 != [0; 16] {
            let mut fetched =
                isocast_surfaces::fetch(service.trim(), token, Duration::from_secs(1))?;
            if fetched.len() != 1 {
                bail!("expected one IOSurface");
            }
            surfaces.insert(id, fetched.remove(0));
        }
        let surface = surfaces.get(&id).context("unknown IOSurface")?;
        let dims = (field(20), field(24), field(28));
        if dims.0 != surface.width()
            || dims.1 != surface.height()
            || field(40) != 0x42475241
            || field(44) != 709
        {
            bail!("camera format/colour mismatch");
        }
        let pts = i64::from_le_bytes(header[32..40].try_into().unwrap());
        if times.last().is_some_and(|&previous| previous >= pts) {
            bail!("non-monotonic camera PTS");
        }
        size = dims;
        latency.push((isocast_surfaces::host_now() as i64 - pts) as f64 / 1e3);
        times.push(pts);
        let io = surface.io_surface();
        // The producer keeps the frame immutable until this reader acks.
        // CPU mapping is for validation only, never in the real transport.
        let row = io.bytes_per_row();
        // ReadOnly is 1. The lock synchronizes CPU visibility with the GPU.
        unsafe extern "C" {
            fn IOSurfaceLock(surface: *const std::ffi::c_void, flags: u32, seed: *mut u32) -> i32;
            fn IOSurfaceUnlock(surface: *const std::ffi::c_void, flags: u32, seed: *mut u32)
            -> i32;
        }
        let raw = (io as *const _ as *const std::ffi::c_void).cast();
        if unsafe { IOSurfaceLock(raw, 1, std::ptr::null_mut()) } != 0 {
            bail!("IOSurfaceLock failed");
        }
        struct Unlock(*const std::ffi::c_void);
        impl Drop for Unlock {
            fn drop(&mut self) {
                unsafe {
                    IOSurfaceUnlock(self.0, 1, std::ptr::null_mut());
                }
            }
        }
        let locked = Unlock(raw);
        let pixels = unsafe {
            std::slice::from_raw_parts(
                io.base_address().cast::<u8>().as_ptr(),
                row * surface.height() as usize,
            )
        };
        let mut hash = 0u64;
        for y in (0..dims.1 as usize).step_by(31) {
            for x in (0..dims.0 as usize).step_by(31) {
                let at = y * row + x * 4;
                hash =
                    hash.wrapping_mul(31).wrapping_add(u32::from_le_bytes(
                        pixels[at..at + 4].try_into().unwrap(),
                    ) as u64);
            }
        }
        hashes.insert(hash);
        if times.len() == 1 {
            for x in [dims.0 / 8, dims.0 / 2, dims.0 * 7 / 8] {
                let at = dims.1 as usize / 2 * row + x as usize * 4;
                colors.push(format!(
                    "[{},{},{},{}]",
                    pixels[at + 2],
                    pixels[at + 1],
                    pixels[at],
                    pixels[at + 3]
                ));
            }
            if let Some(path) = picture {
                let mut ppm = std::fs::File::create(path)?;
                write!(ppm, "P6\n{} {}\n255\n", dims.0, dims.1)?;
                let mut rgb = Vec::with_capacity(dims.0 as usize * dims.1 as usize * 3);
                for y in 0..dims.1 as usize {
                    for x in 0..dims.0 as usize {
                        let at = y * row + x * 4;
                        rgb.extend_from_slice(&[pixels[at + 2], pixels[at + 1], pixels[at]]);
                    }
                }
                ppm.write_all(&rgb)?;
            }
        }
        drop(locked);
        surfaces.remove(&id); // Release the Mach import before the producer reuses it.
        socket.write_all(&[1])?;
    }
    latency.sort_by(f64::total_cmp);
    let count = times.len();
    if count == 0 {
        bail!("no camera frames received");
    }
    let duration = (times[count - 1] - times[0]) as f64 / 1e9;
    let max_gap = times.windows(2).map(|t| t[1] - t[0]).max().unwrap_or(0) as f64 / 1e6;
    println!(
        "{{\"frames\":{count},\"width\":{},\"height\":{},\"fps\":{},\"measured_fps\":{},\"unique_sample_hashes\":{},\"surfaces\":{},\"max_pts_gap_ms\":{max_gap},\"latency_p50_us\":{},\"latency_p99_us\":{},\"bgra_rec709_valid\":true,\"first_rgb_samples\":[{}]}}",
        size.0,
        size.1,
        size.2,
        (count - 1) as f64 / duration.max(1e-9),
        hashes.len(),
        surface_ids.len(),
        latency[count / 2],
        latency[(count * 99 / 100).min(count - 1)],
        colors.join(",")
    );
    Ok(())
}
