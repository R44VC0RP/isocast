//! Single-planar YUYV producer. No kernel-module management: the user supplies
//! the loopback device. No reader-owned buffers or waits for a consumer.
use super::*;
use isocast_engine::record::CameraSink;
use std::io::Write;
use std::sync::{Mutex, Weak};

const OUTPUT: u32 = 2; // V4L2_BUF_TYPE_VIDEO_OUTPUT, also V4L2_CAP_VIDEO_OUTPUT
const READWRITE: u32 = 0x0100_0000;
const LABEL: &str = "Isocast Camera";
const DRIVER: &str = "v4l2 loopback";
const SETUP: &str = "No v4l2loopback camera device. Install v4l2loopback for your kernel (Debian/Ubuntu: v4l2loopback-dkms and matching kernel headers), then run: sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"Isocast Camera\"";

/// An unlabelled/specially labelled fallback belongs to Isocast only for the
/// lifetime of its producer. Discovery must not keep the destination alive.
static ACTIVE: Mutex<Weak<Destination>> = Mutex::new(Weak::new());

struct Destination {
    node: PathBuf,
    display: String,
}

fn claim_destination(node: PathBuf, label: &str) -> Arc<Destination> {
    let dest = Arc::new(Destination {
        display: format!("{label} ({})", node.display()),
        node,
    });
    *ACTIVE.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(&dest);
    dest
}

pub(super) fn is_own_output(node: &Path, driver: &str, label: &str) -> bool {
    driver == DRIVER
        && (label == LABEL
            || ACTIVE
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .upgrade()
                .is_some_and(|d| d.node == node))
}

/// The module publishes per-device `format` and `max_openers` attributes.
/// Don't open real capture devices (or any device) to answer routine status.
pub(crate) fn device() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    for path in nodes() {
        let dir = Path::new("/sys/class/video4linux").join(path.file_name().unwrap());
        if dir.join("max_openers").exists() && dir.join("format").exists() {
            let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
            candidates.push((name.trim().to_owned(), path));
        }
    }
    if candidates.iter().any(|(name, _)| name == LABEL) {
        candidates.retain(|(name, _)| name == LABEL);
    }
    let [(_, path)] = candidates.as_slice() else {
        if candidates.is_empty() {
            bail!(SETUP);
        }
        bail!(
            "Multiple v4l2loopback cameras; give exactly one the card label '{LABEL}' so Isocast never feeds the wrong device"
        );
    };
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: cpath is a live, NUL-terminated /dev/videoN pathname.
    if unsafe { libc::access(cpath.as_ptr(), libc::R_OK | libc::W_OK) } != 0 {
        bail!(
            "Cannot access {}: {}. Give your user read/write access (usually the video group, then log in again); do not run Isocast as root",
            path.display(),
            io::Error::last_os_error()
        );
    }
    Ok(path.clone())
}

pub(crate) fn start(width: u32, height: u32, fps: u32) -> Result<Box<dyn CameraSink<Frame>>> {
    ensure!(
        width > 0 && width.is_multiple_of(2) && height > 0,
        "virtual_camera.width: YUYV needs a positive even width and a positive height"
    );
    let path = device()?;
    let file = open(&path)?; // O_NONBLOCK; this descriptor alone owns the producer.
    let fd = file.as_raw_fd();
    let mut cap: Capability = zeroed();
    xioctl(fd, VIDIOC_QUERYCAP, &mut cap)?;
    ensure!(
        text(&cap.driver) == DRIVER,
        "virtual_camera: {} is not a v4l2loopback device",
        path.display()
    );
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    ensure!(
        caps & OUTPUT != 0 && caps & READWRITE != 0,
        "virtual_camera: {} is not accepting a producer; stop its other producer and try again",
        path.display()
    );
    if caps & CAP_VIDEO_CAPTURE != 0 {
        // Non-exclusive nodes advertise OUTPUT even when another producer
        // is live. Don't overwrite an existing camera feed.
        let format = Path::new("/sys/class/video4linux")
            .join(path.file_name().unwrap())
            .join("format");
        ensure!(
            std::fs::read_to_string(format)?.trim().is_empty(),
            "virtual_camera: {} already has a picture format; stop its producer or use a separate exclusive_caps=1 device",
            path.display()
        );
    }
    // Concurrent Isocast processes must not race QUERYCAP/first write on old drivers.
    file.try_lock()
        .context("virtual_camera: another producer has the camera open")?;

    let mut format: Format = zeroed();
    format.kind = OUTPUT;
    format.fmt.pix = PixFormat {
        width,
        height,
        pixelformat: FOURCC_YUYV,
        field: FIELD_NONE,
        colorspace: 8, // V4L2_COLORSPACE_SRGB, default Y'CbCr matrix is 601, limited range
        ..zeroed()
    };
    xioctl(fd, VIDIOC_S_FMT, &mut format)
        .context("virtual_camera: negotiating YUYV with v4l2loopback")?;
    // SAFETY: single-planar S_FMT returns the pix member.
    let pix = unsafe { format.fmt.pix };
    let stride = pix.bytesperline as usize;
    let len = pix.sizeimage as usize;
    ensure!(
        pix.width == width
            && pix.height == height
            && pix.pixelformat == FOURCC_YUYV
            && stride >= width as usize * 2
            && len >= stride * height as usize
            && len <= MAX_BUFFER_BYTES,
        "virtual_camera: {} refused {width}x{height} YUYV; stop other users of the device or choose a supported size",
        path.display()
    );
    let mut param: StreamParm = zeroed();
    param.kind = OUTPUT;
    // v4l2_outputparm and v4l2_captureparm have identical timeperframe layout.
    param.parm.capture.timeperframe = Fract {
        numerator: 1,
        denominator: fps,
    };
    xioctl(fd, VIDIOC_S_PARM, &mut param)
        .context("virtual_camera.fps: negotiating v4l2loopback rate")?;
    // SAFETY: the single-planar output parameters were initialized by S_PARM.
    let rate = unsafe { param.parm.capture.timeperframe };
    ensure!(
        rate.numerator != 0
            && u64::from(rate.numerator) * u64::from(fps) == u64::from(rate.denominator),
        "virtual_camera.fps: device refused {fps} fps"
    );
    // 0.12.x marks the fd as the writer only on STREAMON; a bare write
    // leaves an exclusive-caps device without OUTPUT even after close.
    // Newer file-I/O drivers require REQBUFS for this ioctl (EINVAL),
    // and acquire/release the stream themselves on first write/close.
    // Hide the node before STREAMON makes it appear capture-capable.
    let destination = claim_destination(path, &text(&cap.card));
    if let Err(e) = xioctl(fd, VIDIOC_STREAMON, &mut (OUTPUT as libc::c_int))
        && !is_einval(&e)
    {
        return Err(e).context("virtual_camera: starting the v4l2loopback output stream");
    }
    Ok(Box::new(Producer {
        file,
        destination,
        width: width as usize,
        stride,
        // S_FMT may return the page-aligned allocation rather than the
        // bytes in the image. A write is one frame, not one allocation.
        next: vec![0; stride * height as usize],
        last: Vec::new(),
    }))
}

struct Producer {
    file: File,
    destination: Arc<Destination>,
    width: usize,
    stride: usize,
    next: Vec<u8>,
    last: Vec<u8>,
}

// BT.601 studio range, pairs of packed BGRA -> Y0 Cb Y1 Cr. Average both
// pixels' chroma before rounding. Ignore alpha: the program is composited.
fn pack_pair(p: &[u8], out: &mut [u8]) {
    let y = |p: &[u8]| {
        ((66 * i32::from(p[2]) + 129 * i32::from(p[1]) + 25 * i32::from(p[0]) + 128) / 256 + 16)
            as u8
    };
    let b = i32::from(p[0]) + i32::from(p[4]);
    let g = i32::from(p[1]) + i32::from(p[5]);
    let r = i32::from(p[2]) + i32::from(p[6]);
    out.copy_from_slice(&[
        y(p),
        (128 + ((112 * b - 74 * g - 38 * r + 256) >> 9)) as u8,
        y(&p[4..]),
        (128 + ((112 * r - 94 * g - 18 * b + 256) >> 9)) as u8,
    ]);
}

fn submit(file: &mut File, bytes: &[u8]) -> Result<bool> {
    match file.write(bytes) {
        Ok(n) if n == bytes.len() => Ok(true),
        Ok(n) => bail!(
            "virtual_camera: v4l2loopback accepted a partial frame ({n}/{} bytes)",
            bytes.len()
        ),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e).context(
            "virtual_camera: writing v4l2loopback frame; check the device then restart the camera",
        ),
    }
}

impl CameraSink<Frame> for Producer {
    fn transport(&self) -> &'static str {
        "v4l2loopback"
    }

    fn destination(&self) -> Option<&str> {
        Some(&self.destination.display)
    }

    fn send(&mut self, frame: &Frame, _: Time) -> Result<bool> {
        let pixels = frame.read_bgra()?;
        for (src, dst) in pixels
            .chunks_exact(self.width * 4)
            .zip(self.next.chunks_exact_mut(self.stride))
        {
            for (pair, out) in src
                .as_chunks::<8>()
                .0
                .iter()
                .zip(dst.as_chunks_mut::<4>().0)
            {
                pack_pair(pair, out);
            }
        }
        let sent = submit(&mut self.file, &self.next)?;
        if sent {
            std::mem::swap(&mut self.next, &mut self.last);
            self.next.resize(self.last.len(), 0);
        }
        Ok(sent)
    }

    fn repeat_last(&mut self) -> Result<bool> {
        if self.last.is_empty() {
            return Ok(false);
        } // started during a pause
        submit(&mut self.file, &self.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_excludes_the_reserved_loopback_and_only_the_live_fallback() {
        let own = Path::new("/dev/video40");
        let other = Path::new("/dev/video41");
        assert!(is_own_output(own, DRIVER, LABEL));
        assert!(!is_own_output(own, "uvcvideo", LABEL));
        let producer = claim_destination(own.into(), "Desk feed");
        assert!(is_own_output(own, DRIVER, "Desk feed"));
        assert!(!is_own_output(other, DRIVER, "Desk feed"));
        drop(producer);
        assert!(!is_own_output(own, DRIVER, "Desk feed"));
    }

    #[test]
    fn yuyv_keeps_luma_order_and_averages_chroma() {
        let mut out = [0; 4];
        pack_pair(&[0, 0, 0, 255, 255, 255, 255, 255], &mut out);
        assert_eq!(out, [16, 128, 235, 128]);
        pack_pair(&[0, 0, 255, 255, 255, 0, 0, 255], &mut out);
        assert_eq!(out, [82, 165, 41, 175]);
    }

    #[test]
    fn holding_repeats_only_the_last_successfully_delivered_bytes() {
        use std::{os::fd::OwnedFd, os::unix::net::UnixDatagram};
        let (tx, rx) = UnixDatagram::pair().unwrap();
        tx.set_nonblocking(true).unwrap();
        rx.set_nonblocking(true).unwrap();
        let mut sink = Producer {
            file: File::from(OwnedFd::from(tx)),
            destination: Arc::new(Destination {
                node: PathBuf::new(),
                display: "test socket".into(),
            }),
            width: 2,
            stride: 4,
            next: vec![0; 4],
            last: Vec::new(),
        };
        let mut pool = FramePool::new(2, 1).unwrap();
        let black = pool.draw(&mut |p, _| p.fill(0)).unwrap().unwrap();
        let white = pool.draw(&mut |p, _| p.fill(255)).unwrap().unwrap();
        let at = Time::from_nanos(0);
        assert!(!sink.repeat_last().unwrap());
        assert!(sink.send(&black, at).unwrap());
        // Fill the consumer's bounded queue, then reject a new white frame.
        while sink.repeat_last().unwrap() {}
        assert!(!sink.send(&white, at).unwrap());
        let mut pixel = [0; 4];
        while rx.recv(&mut pixel).is_ok() {
            assert_eq!(pixel, [16, 128, 16, 128]);
        }
        assert!(sink.repeat_last().unwrap());
        assert_eq!(rx.recv(&mut pixel).unwrap(), 4);
        assert_eq!(pixel, [16, 128, 16, 128]);
        assert!(sink.send(&white, at).unwrap());
        assert_eq!(rx.recv(&mut pixel).unwrap(), 4);
        assert_eq!(pixel, [235, 128, 235, 128]);
    }
}
