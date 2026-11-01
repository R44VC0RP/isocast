//! V4L2 cameras: enumeration from `/sys/class/video4linux` (or `/dev`),
//! MJPEG, YUYV and NV12 capture through mmap streaming, converted on the
//! CPU to BGRA frames from a bounded [`FramePool`]. Works with UVC
//! devices and v4l2loopback. Only formats this module can decode are listed.
//!
//! Device ids are the `/dev/v4l/by-id` link where udev made one (stable
//! across reboots and ports), else the `/dev/videoN` node itself, which
//! may change when devices are plugged in a different order.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use isocast_engine::FramePool as _;
use isocast_engine::{CameraRequest, CaptureFormat, Time, VideoSink};
use isocast_protocol::Canvas;
use isocast_protocol::devices::{Camera, CameraKind, FrameRateRange, VideoFormat};

use crate::frames::{Frame, FramePool};
use crate::system;
use crate::usb_setup;

#[cfg(not(all(
    target_pointer_width = "64",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
compile_error!(
    "V4L2 capture currently supports 64-bit x86_64, aarch64 and riscv64 Linux UAPI layouts"
);

/// Matches the frame pool and compositor limit.
const MAX_DIMENSION: u32 = 8192;
/// Upper bound for one driver buffer: an 8192x8192 BGRA picture.
const MAX_BUFFER_BYTES: usize = 8192 * 8192 * 4;
const BUFFERS: u32 = 4;
const MAX_BUFFERS: u32 = 16;
/// Bounds every driver enumeration loop.
const MAX_ENUM: u32 = 1024;
const MAX_NODES: usize = 256;
const MAX_SIZES: usize = 512;
const POLL_MS: i32 = 100;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BAD_FRAMES: u32 = 60;
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// A driver timestamp further from arrival than this is not trusted.
const MAX_TIMESTAMP_AGE_NS: i64 = 1_000_000_000;
const MAX_TIMESTAMP_FUTURE_NS: i64 = 1_000_000;

const COMMON_SIZES: [(u32, u32); 12] = [
    (320, 240),
    (640, 360),
    (640, 480),
    (800, 600),
    (960, 540),
    (1024, 768),
    (1280, 720),
    (1280, 960),
    (1600, 1200),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
const COMMON_RATES: [u32; 17] = [
    1, 2, 5, 10, 15, 20, 24, 25, 30, 48, 50, 60, 90, 100, 120, 144, 240,
];

// ---------------------------------------------------------------------------
// V4L2 UAPI (linux/videodev2.h), single-planar capture subset.

const fn fourcc(code: &[u8; 4]) -> u32 {
    code[0] as u32 | (code[1] as u32) << 8 | (code[2] as u32) << 16 | (code[3] as u32) << 24
}

const FOURCC_MJPG: u32 = fourcc(b"MJPG");
const FOURCC_JPEG: u32 = fourcc(b"JPEG");
const FOURCC_YUYV: u32 = fourcc(b"YUYV");
const FOURCC_NV12: u32 = fourcc(b"NV12");
/// The formats this module decodes, for messages.
const DECODABLE: &str = "MJPEG, YUYV or NV12";

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_ANY: u32 = 0;
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_TIMEPERFRAME: u32 = 0x1000;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const FRMIVAL_TYPE_DISCRETE: u32 = 1;
const BUF_FLAG_ERROR: u32 = 0x0040;
const BUF_FLAG_TIMESTAMP_MASK: u32 = 0xe000;
const BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// Seconds per frame: `numerator / denominator`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    /// Discrete: width, height. Stepwise: min/max/step width, then height.
    size: [u32; 6],
    reserved: [u32; 2],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrmIvalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    kind: u32,
    /// Discrete: the interval. Stepwise: min, max, step.
    interval: [Fract; 3],
    reserved: [u32; 2],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
union FormatUnion {
    pix: PixFormat,
    raw: [u8; 200],
    // `struct v4l2_window` holds pointers, which aligns the union.
    _align: *mut libc::c_void,
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
struct Format {
    kind: u32,
    fmt: FormatUnion,
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
union ParmUnion {
    capture: CaptureParm,
    raw: [u8; 200],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
struct StreamParm {
    kind: u32,
    parm: ParmUnion,
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct Timecode {
    kind: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
union BufferMemory {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut libc::c_void,
    fd: i32,
}

#[allow(dead_code)] // Kernel ABI layout; not every field is read.
#[repr(C)]
#[derive(Clone, Copy)]
struct Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: BufferMemory,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

// The kernel ABI sizes on 64-bit targets; a mismatch would make every
// ioctl number (and the driver's copy) wrong.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<FmtDesc>() == 64);
    assert!(size_of::<FrmSizeEnum>() == 44);
    assert!(size_of::<FrmIvalEnum>() == 52);
    assert!(size_of::<Format>() == 208);
    assert!(size_of::<StreamParm>() == 204);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Buffer>() == 88);
};

const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const fn ioc(direction: u64, number: u64, size: usize) -> u64 {
    (direction << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | number
}

const VIDIOC_QUERYCAP: u64 = ioc(IOC_READ, 0, size_of::<Capability>());
const VIDIOC_ENUM_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<FmtDesc>());
const VIDIOC_G_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 4, size_of::<Format>());
const VIDIOC_S_FMT: u64 = ioc(IOC_READ | IOC_WRITE, 5, size_of::<Format>());
const VIDIOC_REQBUFS: u64 = ioc(IOC_READ | IOC_WRITE, 8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: u64 = ioc(IOC_READ | IOC_WRITE, 9, size_of::<Buffer>());
const VIDIOC_QBUF: u64 = ioc(IOC_READ | IOC_WRITE, 15, size_of::<Buffer>());
const VIDIOC_DQBUF: u64 = ioc(IOC_READ | IOC_WRITE, 17, size_of::<Buffer>());
const VIDIOC_STREAMON: u64 = ioc(IOC_WRITE, 18, size_of::<libc::c_int>());
const VIDIOC_STREAMOFF: u64 = ioc(IOC_WRITE, 19, size_of::<libc::c_int>());
const VIDIOC_G_PARM: u64 = ioc(IOC_READ | IOC_WRITE, 21, size_of::<StreamParm>());
const VIDIOC_S_PARM: u64 = ioc(IOC_READ | IOC_WRITE, 22, size_of::<StreamParm>());
const VIDIOC_ENUM_FRAMESIZES: u64 = ioc(IOC_READ | IOC_WRITE, 74, size_of::<FrmSizeEnum>());
const VIDIOC_ENUM_FRAMEINTERVALS: u64 = ioc(IOC_READ | IOC_WRITE, 75, size_of::<FrmIvalEnum>());

/// One ioctl, retried when a signal interrupts it.
fn xioctl<T>(fd: RawFd, request: u64, argument: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: every request number above encodes `size_of::<T>()` of
        // the matching repr(C) argument, which outlives the call.
        let result = unsafe { libc::ioctl(fd, request as _, argument as *mut T) };
        if result != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn zeroed<T>() -> T {
    // SAFETY: only used for the plain-old-data UAPI structs above, for
    // which all-zero bytes are valid (null pointers in unions included).
    unsafe { std::mem::zeroed() }
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn fourcc_name(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() {
                char::from(*b)
            } else {
                '?'
            }
        })
        .collect()
}

fn is_einval(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::EINVAL)
}

// ---------------------------------------------------------------------------
// Formats.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pixel {
    Mjpeg,
    Yuyv,
    /// Single-planar 4:2:0: a luma plane, then interleaved Cb/Cr rows at
    /// the same stride.
    Nv12,
}

impl Pixel {
    fn from_fourcc(code: u32) -> Option<Self> {
        match code {
            FOURCC_MJPG | FOURCC_JPEG => Some(Self::Mjpeg),
            FOURCC_YUYV => Some(Self::Yuyv),
            FOURCC_NV12 => Some(Self::Nv12),
            _ => None,
        }
    }

    /// Preference at equal size and rate: uncompressed before MJPEG, 4:2:2
    /// YUYV before 4:2:0 NV12.
    fn rank(self) -> u8 {
        match self {
            Self::Yuyv => 0,
            Self::Nv12 => 1,
            Self::Mjpeg => 2,
        }
    }

    /// Whether this backend converts a picture this size: 4:2:2 needs an even
    /// width, 4:2:0 also an even height.
    fn fits(self, width: u32, height: u32) -> bool {
        match self {
            Self::Yuyv => width.is_multiple_of(2),
            Self::Nv12 => width.is_multiple_of(2) && height.is_multiple_of(2),
            Self::Mjpeg => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Rates {
    Discrete(Vec<Fract>),
    /// Any interval from `fastest` to `slowest`.
    Range {
        fastest: Fract,
        slowest: Fract,
    },
    /// The driver does not say (some v4l2loopback producers).
    Unknown,
}

/// One decodable size of one pixel format, and its frame rates.
#[derive(Clone, Debug, PartialEq)]
struct Size {
    fourcc: u32,
    width: u32,
    height: u32,
    rates: Rates,
}

/// One concrete capture mode a source could ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    fourcc: u32,
    pixel: Pixel,
    width: u32,
    height: u32,
    interval: Option<Fract>,
    /// Nominal integer rate; `None` when the driver doesn't report one.
    fps: Option<u32>,
}

fn rate(interval: Fract) -> Option<f64> {
    (interval.numerator > 0 && interval.denominator > 0)
        .then(|| f64::from(interval.denominator) / f64::from(interval.numerator))
}

/// The integer rate an interval runs at (30000/1001 s⁻¹ is 30), if it is
/// within 0.5% of one from 1 to 240.
fn nominal(interval: Fract) -> Option<u32> {
    let (numerator, denominator) = (
        u64::from(interval.numerator),
        u64::from(interval.denominator),
    );
    if numerator == 0 || denominator == 0 {
        return None;
    }
    let fps = (denominator + numerator / 2) / numerator;
    if !(1..=240).contains(&fps) {
        return None;
    }
    let exact = fps * numerator;
    (denominator.abs_diff(exact) * 1000 <= exact * 5).then_some(fps as u32)
}

fn stepwise_sizes(range: [u32; 6]) -> Vec<(u32, u32)> {
    let [min_w, max_w, step_w, min_h, max_h, step_h] = range;
    let fits = |value: u32, min: u32, max: u32, step: u32| {
        (min.max(1)..=max.min(MAX_DIMENSION)).contains(&value)
            && (step <= 1 || (value - min).is_multiple_of(step))
    };
    let mut sizes: Vec<(u32, u32)> = COMMON_SIZES
        .into_iter()
        .chain([(max_w, max_h), (min_w, min_h)])
        .filter(|&(w, h)| fits(w, min_w, max_w, step_w) && fits(h, min_h, max_h, step_h))
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

fn current_format(fd: RawFd) -> Option<PixFormat> {
    let mut format: Format = zeroed();
    format.kind = BUF_TYPE_VIDEO_CAPTURE;
    xioctl(fd, VIDIOC_G_FMT, &mut format).ok()?;
    // SAFETY: a capture-type G_FMT fills the `pix` member.
    Some(unsafe { format.fmt.pix })
}

fn current_parm(fd: RawFd) -> Option<CaptureParm> {
    let mut parm: StreamParm = zeroed();
    parm.kind = BUF_TYPE_VIDEO_CAPTURE;
    xioctl(fd, VIDIOC_G_PARM, &mut parm).ok()?;
    // SAFETY: a capture-type G_PARM fills the `capture` member.
    Some(unsafe { parm.parm.capture })
}

fn frame_sizes(fd: RawFd, code: u32) -> Result<Vec<(u32, u32)>> {
    let mut sizes = Vec::new();
    for index in 0..MAX_ENUM {
        let mut size: FrmSizeEnum = zeroed();
        size.index = index;
        size.pixel_format = code;
        if let Err(error) = xioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut size) {
            if index == 0 && !is_einval(&error) && error.raw_os_error() != Some(libc::ENOTTY) {
                return Err(error).context("camera.formats: VIDIOC_ENUM_FRAMESIZES");
            }
            break;
        }
        if size.kind != FRMSIZE_TYPE_DISCRETE {
            // Continuous and stepwise ranges come back once, at index 0.
            return Ok(stepwise_sizes(size.size));
        }
        sizes.push((size.size[0], size.size[1]));
    }
    // No size list (v4l2loopback, some capture cards): the current
    // format is the one the device delivers.
    if sizes.is_empty()
        && let Some(pix) = current_format(fd).filter(|pix| pix.pixelformat == code)
    {
        sizes.push((pix.width, pix.height));
    }
    Ok(sizes)
}

fn frame_rates(fd: RawFd, code: u32, width: u32, height: u32) -> Rates {
    let mut list = Vec::new();
    for index in 0..MAX_ENUM {
        let mut interval: FrmIvalEnum = zeroed();
        interval.index = index;
        interval.pixel_format = code;
        interval.width = width;
        interval.height = height;
        if xioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut interval).is_err() {
            break;
        }
        if interval.kind != FRMIVAL_TYPE_DISCRETE {
            let [fastest, slowest, _] = interval.interval;
            if rate(fastest).is_some() && rate(slowest).is_some() {
                return Rates::Range { fastest, slowest };
            }
            break;
        }
        if rate(interval.interval[0]).is_some() {
            list.push(interval.interval[0]);
        }
    }
    if !list.is_empty() {
        return Rates::Discrete(list);
    }
    let current = current_format(fd)
        .is_some_and(|pix| pix.pixelformat == code && pix.width == width && pix.height == height);
    match current_parm(fd).map(|parm| parm.timeperframe) {
        Some(interval) if current && rate(interval).is_some() => Rates::Discrete(vec![interval]),
        _ => Rates::Unknown,
    }
}

/// Every decodable size the device offers, with its rates.
fn sizes(fd: RawFd) -> Result<Vec<Size>> {
    let mut sizes = Vec::new();
    for index in 0..MAX_ENUM {
        let mut desc: FmtDesc = zeroed();
        desc.index = index;
        desc.kind = BUF_TYPE_VIDEO_CAPTURE;
        if let Err(error) = xioctl(fd, VIDIOC_ENUM_FMT, &mut desc) {
            if is_einval(&error) {
                break;
            }
            return Err(error).context("camera.formats: VIDIOC_ENUM_FMT");
        }
        let Some(pixel) = Pixel::from_fourcc(desc.pixelformat) else {
            continue;
        };
        for (width, height) in frame_sizes(fd, desc.pixelformat)? {
            if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
                continue;
            }
            if !pixel.fits(width, height) {
                continue;
            }
            if sizes.len() == MAX_SIZES {
                return Ok(sizes);
            }
            sizes.push(Size {
                fourcc: desc.pixelformat,
                width,
                height,
                rates: frame_rates(fd, desc.pixelformat, width, height),
            });
        }
    }
    Ok(sizes)
}

fn modes(sizes: &[Size]) -> Vec<Mode> {
    let mut modes = Vec::new();
    for size in sizes {
        let Some(pixel) = Pixel::from_fourcc(size.fourcc) else {
            continue;
        };
        let mode = |interval: Option<Fract>, fps: Option<u32>| Mode {
            fourcc: size.fourcc,
            pixel,
            width: size.width,
            height: size.height,
            interval,
            fps,
        };
        match &size.rates {
            Rates::Discrete(list) => {
                for &interval in list {
                    if let Some(fps) = nominal(interval) {
                        modes.push(mode(Some(interval), Some(fps)));
                    }
                }
            }
            Rates::Range { fastest, slowest } => {
                let (Some(high), Some(low)) = (rate(*fastest), rate(*slowest)) else {
                    continue;
                };
                for fps in COMMON_RATES {
                    let value = f64::from(fps);
                    if (low - 1e-6..=high + 1e-6).contains(&value) {
                        let interval = Fract {
                            numerator: 1,
                            denominator: fps,
                        };
                        modes.push(mode(Some(interval), Some(fps)));
                    }
                }
            }
            Rates::Unknown => modes.push(mode(None, None)),
        }
    }
    modes
}

fn video_formats(sizes: &[Size]) -> Vec<VideoFormat> {
    let mut formats: Vec<VideoFormat> = sizes
        .iter()
        .map(|size| {
            let (mut rates, mut ranges) = match &size.rates {
                Rates::Discrete(list) => {
                    let rates: Vec<f64> = list.iter().filter_map(|i| rate(*i)).collect();
                    let ranges: Vec<FrameRateRange> = rates
                        .iter()
                        .map(|&r| FrameRateRange { min: r, max: r })
                        .collect();
                    (rates, ranges)
                }
                Rates::Range { fastest, slowest } => match (rate(*slowest), rate(*fastest)) {
                    (Some(min), Some(max)) => (vec![min, max], vec![FrameRateRange { min, max }]),
                    _ => (Vec::new(), Vec::new()),
                },
                Rates::Unknown => (Vec::new(), Vec::new()),
            };
            rates.sort_by(f64::total_cmp);
            rates.dedup();
            ranges.sort_by(|a, b| b.max.total_cmp(&a.max));
            ranges.dedup();
            VideoFormat {
                width: size.width,
                height: size.height,
                pixel_format: fourcc_name(size.fourcc),
                frame_rates: rates,
                frame_rate_ranges: ranges,
            }
        })
        .collect();
    formats.sort_by(|a, b| {
        (u64::from(b.width) * u64::from(b.height), b.width)
            .cmp(&(u64::from(a.width) * u64::from(a.height), a.width))
            .then_with(|| a.pixel_format.cmp(&b.pixel_format))
    });
    formats.dedup_by(|a, b| {
        a.width == b.width && a.height == b.height && a.pixel_format == b.pixel_format
    });
    formats
}

/// The mode `request` asks for, or the nearest to `canvas`. Requested
/// fields are constraints; open ones prefer the canvas size, then rate,
/// then [`Pixel::rank`].
fn select(modes: &[Mode], request: CameraRequest, canvas: &Canvas) -> Result<Mode> {
    let wanted = request.size.unwrap_or((canvas.width, canvas.height));
    let fps = request.fps.unwrap_or(canvas.fps);
    let selected = modes
        .iter()
        .filter(|mode| {
            request
                .size
                .is_none_or(|size| size == (mode.width, mode.height))
                && request
                    .fps
                    .is_none_or(|want| mode.fps.is_none_or(|have| have == want))
        })
        .min_by_key(|mode| {
            (
                u64::from(mode.width.abs_diff(wanted.0))
                    + u64::from(mode.height.abs_diff(wanted.1)),
                mode.fps.map_or(1000, |have| have.abs_diff(fps)),
                mode.pixel.rank(),
            )
        });
    if let Some(mode) = selected {
        return Ok(*mode);
    }
    let mut choices: Vec<String> = modes
        .iter()
        .map(|mode| {
            format!(
                "{}x{}@{} {}",
                mode.width,
                mode.height,
                mode.fps.map_or("?".into(), |fps| fps.to_string()),
                fourcc_name(mode.fourcc)
            )
        })
        .collect();
    choices.dedup();
    bail!(
        "camera.size/fps: request {request:?} has no {DECODABLE} match; choices: {}",
        if choices.is_empty() {
            "(none)".to_string()
        } else {
            choices.join(", ")
        }
    )
}

// ---------------------------------------------------------------------------
// Devices.

struct Identity {
    node: PathBuf,
    caps: u32,
    camera: Camera,
}

pub fn access() -> Option<bool> {
    access_to(&nodes())
}

/// Whether this user can open any of `nodes` for reading and writing
/// (`None` without nodes). One is enough: `devices` skips the others.
fn access_to(nodes: &[PathBuf]) -> Option<bool> {
    (!nodes.is_empty()).then(|| {
        nodes.iter().any(|node| {
            std::ffi::CString::new(node.as_os_str().as_encoded_bytes()).is_ok_and(|path| {
                // SAFETY: checks access to a NUL-terminated path; no side effects.
                unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
            })
        })
    })
}

/// Capture nodes, `/dev/videoN` ordered by N.
fn nodes() -> Vec<PathBuf> {
    let number = |name: &str| name.strip_prefix("video")?.parse::<u32>().ok();
    let scan = |dir: &str| -> Vec<u32> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| number(entry.ok()?.file_name().to_str()?))
                    .take(MAX_NODES)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut numbers = scan("/sys/class/video4linux");
    if numbers.is_empty() {
        // No sysfs (some containers): the device nodes themselves.
        numbers = scan("/dev");
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers
        .into_iter()
        .map(|n| PathBuf::from(format!("/dev/video{n}")))
        .collect()
}

/// `/dev/videoN` → its `/dev/v4l/by-id` link, where udev made one.
fn stable_ids() -> HashMap<PathBuf, String> {
    let mut ids: HashMap<PathBuf, String> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/dev/v4l/by-id") else {
        return ids;
    };
    for entry in entries.filter_map(Result::ok).take(MAX_ENUM as usize) {
        let link = entry.path();
        let (Ok(node), Some(id)) = (std::fs::canonicalize(&link), link.to_str()) else {
            continue;
        };
        let id = id.to_string();
        ids.entry(node)
            .and_modify(|existing| {
                if id < *existing {
                    existing.clone_from(&id);
                }
            })
            .or_insert(id);
    }
    ids
}

fn sysfs(node: &Path, attribute: &str) -> Option<String> {
    let name = node.file_name()?.to_str()?;
    // `device` is the interface; its parent the USB device.
    let path = format!("/sys/class/video4linux/{name}/device/../{attribute}");
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty() && value.len() <= 256).then(|| value.to_string())
}

fn kind(node: &Path, driver: &str, bus: &str) -> CameraKind {
    if driver.contains("loopback") || bus.contains("v4l2loopback") {
        return CameraKind::Virtual;
    }
    match sysfs(node, "removable").as_deref() {
        Some("fixed") => CameraKind::BuiltIn,
        Some("removable") => CameraKind::External,
        _ if bus.starts_with("usb") => CameraKind::External,
        _ => CameraKind::Other,
    }
}

fn open(node: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(node)
        .with_context(|| format!("camera.device: opening {}", node.display()))?;
    ensure!(
        file.metadata()?.file_type().is_char_device(),
        "camera.device: {} is not a character device",
        node.display()
    );
    Ok(file)
}

fn probe(node: &Path, ids: &HashMap<PathBuf, String>) -> Result<Option<(Identity, File)>> {
    let file = open(node)?;
    let mut cap: Capability = zeroed();
    xioctl(file.as_raw_fd(), VIDIOC_QUERYCAP, &mut cap)
        .with_context(|| format!("camera.device: VIDIOC_QUERYCAP on {}", node.display()))?;
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    // Metadata and output-only nodes (UVC's second node, a loopback
    // without a producer) can't deliver pictures.
    if caps & CAP_VIDEO_CAPTURE == 0 || caps & CAP_STREAMING == 0 {
        return Ok(None);
    }
    let driver = text(&cap.driver);
    let bus = text(&cap.bus_info);
    let mut name = text(&cap.card);
    if name.is_empty() {
        name = node.display().to_string();
    }
    let id = ids
        .get(node)
        .cloned()
        .unwrap_or_else(|| node.display().to_string());
    let camera = Camera {
        id,
        model: sysfs(node, "product").unwrap_or_else(|| driver.clone()),
        manufacturer: sysfs(node, "manufacturer").unwrap_or_default(),
        kind: kind(node, &driver, &bus),
        name,
        formats: Vec::new(),
        suspended: false,
    };
    Ok(Some((
        Identity {
            node: node.to_path_buf(),
            caps,
            camera,
        },
        file,
    )))
}

/// A capture node is a camera only if it offers a format this backend
/// decodes; the Logitech BRIO's infrared node (GREY only, named like the
/// camera) doesn't. Nodes whose formats can't be read still count.
fn is_camera(sizes: &Result<Vec<Size>>) -> bool {
    !matches!(sizes, Ok(sizes) if sizes.is_empty())
}

/// Every pixel format a node offers, for messages.
fn offered_formats(fd: RawFd) -> Vec<String> {
    let mut formats = Vec::new();
    for index in 0..MAX_ENUM {
        let mut desc: FmtDesc = zeroed();
        desc.index = index;
        desc.kind = BUF_TYPE_VIDEO_CAPTURE;
        if xioctl(fd, VIDIOC_ENUM_FMT, &mut desc).is_err() {
            break;
        }
        formats.push(fourcc_name(desc.pixelformat));
    }
    formats
}

/// Every V4L2 capture device and the formats this backend decodes.
/// Devices that can't be opened (permissions) are skipped and logged.
pub fn devices() -> Result<Vec<Camera>> {
    let ids = stable_ids();
    let mut cameras = Vec::new();
    for node in nodes() {
        match probe(&node, &ids) {
            Ok(Some((identity, file))) => {
                let mut camera = identity.camera;
                let sizes = sizes(file.as_raw_fd());
                if !is_camera(&sizes) {
                    isocast_diagnostics::output::deferred(
                        "info",
                        module_path!(),
                        format!(
                            "Linux camera devices: skipping {:?} ({}): no {DECODABLE} \
                             format; it offers {}",
                            camera.name,
                            node.display(),
                            offered_formats(file.as_raw_fd()).join(", ")
                        ),
                    );
                    continue;
                }
                match sizes {
                    Ok(sizes) => camera.formats = video_formats(&sizes),
                    Err(error) => isocast_diagnostics::output::deferred(
                        "warn",
                        module_path!(),
                        format!(
                            "Linux camera devices: {:?} ({}): {error:#}",
                            camera.name, camera.id
                        ),
                    ),
                }
                cameras.push(camera);
            }
            Ok(None) => {}
            Err(error) => isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!(
                    "Linux camera devices: skipping {}: {error:#}",
                    node.display()
                ),
            ),
        }
    }
    Ok(cameras)
}

pub fn probe_capture(
    device: &str,
    request: CameraRequest,
    canvas: &Canvas,
) -> Result<isocast_engine::Probe> {
    let (identity, file) = resolve(device)?;
    let mode = select(&modes(&sizes(file.as_raw_fd())?), request, canvas)?;
    Ok(isocast_engine::Probe {
        device: identity.camera.id,
        video: mode.fps.map(|fps| CaptureFormat {
            width: mode.width,
            height: mode.height,
            fps,
        }),
        channels: None,
    })
}

fn resolve(query: &str) -> Result<(Identity, File)> {
    let ids = stable_ids();
    let mut found = Vec::new();
    for node in nodes() {
        // Only what `devices` lists.
        if let Ok(Some((identity, file))) = probe(&node, &ids)
            && is_camera(&sizes(file.as_raw_fd()))
        {
            found.push(identity);
        }
    }
    let Some(index) = pick(&found, query) else {
        let choices = found
            .iter()
            .map(|identity| format!("{} ({})", identity.camera.name, identity.camera.id))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "camera.device: {query:?} matches no single camera; use an id, a /dev/video node, an exact name or an unambiguous part of one; valid choices: {}",
            if choices.is_empty() {
                "(none)"
            } else {
                &choices
            }
        );
    };
    let identity = found.swap_remove(index);
    let file = open(&identity.node)?;
    Ok((identity, file))
}

/// The one camera in `found` that `query` names: its id, its node, its
/// exact name, or an unambiguous part of its name.
fn pick(found: &[Identity], query: &str) -> Option<usize> {
    let canonical = query
        .starts_with('/')
        .then(|| std::fs::canonicalize(query).ok())
        .flatten();
    found
        .iter()
        .position(|identity| identity.camera.id == query)
        .or_else(|| {
            found.iter().position(|identity| {
                identity.node.as_os_str() == query || canonical.as_ref() == Some(&identity.node)
            })
        })
        .or_else(|| {
            let lower = query.to_lowercase();
            if query.is_empty() || query.contains('\0') || query.eq_ignore_ascii_case("default") {
                return None;
            }
            let named = |exact: bool| -> Vec<usize> {
                found
                    .iter()
                    .enumerate()
                    .filter(|(_, identity)| {
                        let name = identity.camera.name.to_lowercase();
                        if exact {
                            name == lower
                        } else {
                            name.contains(&lower)
                        }
                    })
                    .map(|(index, _)| index)
                    .collect()
            };
            let exact = named(true);
            let matches = if exact.is_empty() {
                named(false)
            } else {
                exact
            };
            (matches.len() == 1).then(|| matches[0])
        })
}

// ---------------------------------------------------------------------------
// Color conversion.

/// Y'CbCr → R'G'B' in 16.16 fixed point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Yuv {
    y_offset: i32,
    y_scale: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

impl Yuv {
    const BT601: (f64, f64) = (0.299, 0.114);
    const BT709: (f64, f64) = (0.2126, 0.0722);
    const BT2020: (f64, f64) = (0.2627, 0.0593);

    fn new((kr, kb): (f64, f64), full: bool) -> Self {
        let kg = 1.0 - kr - kb;
        let (y_scale, c_scale, y_offset) = if full {
            (1.0, 1.0, 0)
        } else {
            (255.0 / 219.0, 255.0 / 224.0, 16)
        };
        let fixed = |value: f64| (value * 65536.0).round() as i32;
        Self {
            y_offset,
            y_scale: fixed(y_scale),
            rv: fixed(2.0 * (1.0 - kr) * c_scale),
            gu: fixed(2.0 * kb * (1.0 - kb) / kg * c_scale),
            gv: fixed(2.0 * kr * (1.0 - kr) / kg * c_scale),
            bu: fixed(2.0 * (1.0 - kb) * c_scale),
        }
    }

    /// From the driver's colorimetry, with V4L2's defaults: the encoding
    /// follows the colorspace (BT.601 unless Rec. 709 or BT.2020), and
    /// Y'CbCr is limited range except for the JPEG colorspace.
    fn from_format(colorspace: u32, ycbcr_enc: u32, quantization: u32) -> Self {
        let matrix = match ycbcr_enc {
            1 | 3 | 5 => Self::BT601,
            2 | 4 => Self::BT709,
            6 | 7 => Self::BT2020,
            _ => match colorspace {
                3 | 12 => Self::BT709,
                10 => Self::BT2020,
                _ => Self::BT601,
            },
        };
        let full = match quantization {
            1 => true,
            2 => false,
            _ => colorspace == 7,
        };
        Self::new(matrix, full)
    }

    #[inline]
    fn bgra(&self, y: u8, u: u8, v: u8) -> [u8; 4] {
        let y = (i32::from(y) - self.y_offset) * self.y_scale + 32768;
        let u = i32::from(u) - 128;
        let v = i32::from(v) - 128;
        let clamp = |value: i32| (value >> 16).clamp(0, 255) as u8;
        [
            clamp(y + self.bu * u),
            clamp(y - self.gu * u - self.gv * v),
            clamp(y + self.rv * v),
            255,
        ]
    }
}

/// Bytes a picture of `rows` rows of `row` bytes at `stride` occupies.
fn extent(stride: usize, row: usize, rows: usize) -> Option<usize> {
    stride.checked_mul(rows.checked_sub(1)?)?.checked_add(row)
}

fn yuyv_to_bgra(
    source: &[u8],
    source_stride: usize,
    width: usize,
    height: usize,
    yuv: &Yuv,
    target: &mut [u8],
    target_stride: usize,
) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width.is_multiple_of(2),
        "camera.yuyv: {width}x{height} must be positive with an even width"
    );
    let row = width * 2;
    let out = width * 4;
    ensure!(
        source_stride >= row && target_stride >= out,
        "camera.yuyv: stride {source_stride} is less than a {width}-pixel row"
    );
    let need = extent(source_stride, row, height).context("camera.yuyv: frame size overflow")?;
    ensure!(
        source.len() >= need,
        "camera.yuyv: frame has {} bytes, {width}x{height} at stride {source_stride} needs {need}",
        source.len()
    );
    let need = extent(target_stride, out, height).context("camera.yuyv: frame size overflow")?;
    ensure!(target.len() >= need, "camera.yuyv: target frame too small");
    for (line, target) in source
        .chunks(source_stride)
        .zip(target.chunks_mut(target_stride))
        .take(height)
    {
        for (pair, pixels) in line[..row]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(target[..out].as_chunks_mut::<8>().0)
        {
            pixels[..4].copy_from_slice(&yuv.bgra(pair[0], pair[1], pair[3]));
            pixels[4..].copy_from_slice(&yuv.bgra(pair[2], pair[1], pair[3]));
        }
    }
    Ok(())
}

/// Bytes a single-planar NV12 picture occupies; the last chroma row needs
/// no padding.
fn nv12_extent(stride: usize, width: usize, height: usize) -> Option<usize> {
    stride
        .checked_mul(height)?
        .checked_add(extent(stride, width, height / 2)?)
}

fn nv12_to_bgra(
    source: &[u8],
    source_stride: usize,
    width: usize,
    height: usize,
    yuv: &Yuv,
    target: &mut [u8],
    target_stride: usize,
) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width.is_multiple_of(2) && height.is_multiple_of(2),
        "camera.nv12: {width}x{height} must be positive with an even width and height"
    );
    let out = width * 4;
    ensure!(
        source_stride >= width && target_stride >= out,
        "camera.nv12: stride {source_stride} is less than a {width}-pixel row"
    );
    let need =
        nv12_extent(source_stride, width, height).context("camera.nv12: frame size overflow")?;
    ensure!(
        source.len() >= need,
        "camera.nv12: frame has {} bytes, {width}x{height} at stride {source_stride} needs {need}",
        source.len()
    );
    let need = extent(target_stride, out, height).context("camera.nv12: frame size overflow")?;
    ensure!(target.len() >= need, "camera.nv12: target frame too small");
    let (luma, chroma) = source.split_at(source_stride * height);
    for (row, target) in target.chunks_mut(target_stride).take(height).enumerate() {
        let y = &luma[row * source_stride..][..width];
        let uv = &chroma[row / 2 * source_stride..][..width];
        for ((y, uv), pixels) in y
            .as_chunks::<2>()
            .0
            .iter()
            .zip(uv.as_chunks::<2>().0)
            .zip(target[..out].as_chunks_mut::<8>().0)
        {
            pixels[..4].copy_from_slice(&yuv.bgra(y[0], uv[0], uv[1]));
            pixels[4..].copy_from_slice(&yuv.bgra(y[1], uv[0], uv[1]));
        }
    }
    Ok(())
}

fn rgb_to_bgra(
    source: &[u8],
    channels: usize,
    width: usize,
    height: usize,
    target: &mut [u8],
    target_stride: usize,
) -> Result<()> {
    ensure!(
        matches!(channels, 1 | 3),
        "camera.mjpeg: {channels} channels"
    );
    let row = width * channels;
    let out = width * 4;
    ensure!(
        width > 0 && height > 0 && source.len() >= row * height && target_stride >= out,
        "camera.mjpeg: decoded picture does not match {width}x{height}"
    );
    let need = extent(target_stride, out, height).context("camera.mjpeg: frame size overflow")?;
    ensure!(target.len() >= need, "camera.mjpeg: target frame too small");
    for (line, target) in source
        .chunks_exact(row)
        .zip(target.chunks_mut(target_stride))
        .take(height)
    {
        let target = &mut target[..out];
        if channels == 3 {
            for (rgb, bgra) in line
                .as_chunks::<3>()
                .0
                .iter()
                .zip(target.as_chunks_mut::<4>().0)
            {
                bgra.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
        } else {
            for (luma, bgra) in line.iter().zip(target.as_chunks_mut::<4>().0) {
                bgra.copy_from_slice(&[*luma, *luma, *luma, 255]);
            }
        }
    }
    Ok(())
}

// Annex K tables. MJPEG cameras commonly omit DHT and rely on them.
const DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

fn default_huffman_segment() -> Vec<u8> {
    let tables: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &DC_LUMA_BITS, &DC_VALUES),
        (0x10, &AC_LUMA_BITS, &AC_LUMA_VALUES),
        (0x01, &DC_CHROMA_BITS, &DC_VALUES),
        (0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALUES),
    ];
    let length = 2 + tables
        .iter()
        .map(|(_, bits, values)| 1 + bits.len() + values.len())
        .sum::<usize>();
    let mut segment = vec![0xff, 0xc4];
    segment.extend_from_slice(&(length as u16).to_be_bytes());
    for (class, bits, values) in tables {
        segment.push(class);
        segment.extend_from_slice(bits);
        segment.extend_from_slice(values);
    }
    segment
}

/// Where the start of scan is, when no DHT segment comes before it.
/// Walks the marker segments only; never reads past `data`.
fn missing_huffman(data: &[u8]) -> Result<Option<usize>> {
    ensure!(
        data.len() >= 4 && data[0] == 0xff && data[1] == 0xd8,
        "camera.mjpeg: frame does not start with a JPEG SOI marker"
    );
    let mut at = 2;
    let mut tables = false;
    loop {
        ensure!(
            at + 1 < data.len() && data[at] == 0xff,
            "camera.mjpeg: truncated or corrupt header at byte {at}"
        );
        let mut marker = at + 1;
        while marker < data.len() && data[marker] == 0xff {
            marker += 1;
        }
        ensure!(marker < data.len(), "camera.mjpeg: truncated header");
        match data[marker] {
            0xda => return Ok((!tables).then_some(at)),
            0xd0..=0xd7 | 0x01 => {
                at = marker + 1;
                continue;
            }
            0xd9 => bail!("camera.mjpeg: end of image before the first scan"),
            0xc4 => tables = true,
            _ => {}
        }
        ensure!(marker + 2 < data.len(), "camera.mjpeg: truncated segment");
        let length = usize::from(u16::from_be_bytes([data[marker + 1], data[marker + 2]]));
        ensure!(length >= 2, "camera.mjpeg: segment length {length}");
        at = marker + 1 + length;
        ensure!(
            at <= data.len(),
            "camera.mjpeg: segment runs past the frame"
        );
    }
}

#[derive(Default)]
struct Jpeg {
    pixels: Vec<u8>,
    patched: Vec<u8>,
}

/// Decodes one MJPEG frame of exactly `width`x`height` into
/// `jpeg.pixels`; returns its channel count (3 RGB, 1 gray).
fn decode_mjpeg(data: &[u8], width: u32, height: u32, jpeg: &mut Jpeg) -> Result<usize> {
    use image::ImageDecoder as _;

    let input: &[u8] = match missing_huffman(data)? {
        Some(scan) => {
            jpeg.patched.clear();
            jpeg.patched.extend_from_slice(&data[..scan]);
            jpeg.patched.extend_from_slice(&default_huffman_segment());
            jpeg.patched.extend_from_slice(&data[scan..]);
            &jpeg.patched
        }
        None => data,
    };
    let pixels = &mut jpeg.pixels;
    catch_unwind(AssertUnwindSafe(|| -> Result<usize> {
        let mut decoder = image::codecs::jpeg::JpegDecoder::new(io::Cursor::new(input))
            .context("camera.mjpeg: reading header")?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        limits.max_alloc = Some(MAX_BUFFER_BYTES as u64 * 2);
        decoder.set_limits(limits).context("camera.mjpeg: limits")?;
        let size = decoder.dimensions();
        ensure!(
            size == (width, height),
            "camera.mjpeg: frame is {}x{}, the format is {width}x{height}",
            size.0,
            size.1
        );
        let channels = match decoder.color_type() {
            image::ColorType::Rgb8 => 3,
            image::ColorType::L8 => 1,
            other => bail!("camera.mjpeg: unsupported decoded color type {other:?}"),
        };
        let len = usize::try_from(decoder.total_bytes())?;
        ensure!(
            len == width as usize * height as usize * channels,
            "camera.mjpeg: decoder size mismatch"
        );
        pixels.clear();
        pixels.try_reserve_exact(len)?;
        pixels.resize(len, 0);
        decoder
            .read_image(pixels)
            .context("camera.mjpeg: decoding")?;
        Ok(channels)
    }))
    .map_err(|_| anyhow::anyhow!("camera.mjpeg: decoder panicked on a corrupt frame"))?
}

#[derive(Clone, Copy, Debug)]
struct Layout {
    pixel: Pixel,
    width: u32,
    height: u32,
    /// Bytes per row (per luma and chroma row for NV12); 0 for MJPEG.
    stride: usize,
    yuv: Yuv,
}

/// One driver picture as a BGRA frame from `pool`; `None` when every
/// pooled frame is still in use downstream.
fn draw_frame(
    pool: &mut FramePool,
    layout: &Layout,
    data: &[u8],
    jpeg: &mut Jpeg,
) -> Result<Option<Frame>> {
    let (width, height) = (layout.width as usize, layout.height as usize);
    let mut failure = None;
    let frame = match layout.pixel {
        Pixel::Yuyv | Pixel::Nv12 => {
            let convert = if layout.pixel == Pixel::Yuyv {
                yuyv_to_bgra
            } else {
                nv12_to_bgra
            };
            pool.draw(&mut |pixels, stride| {
                if let Err(error) = convert(
                    data,
                    layout.stride,
                    width,
                    height,
                    &layout.yuv,
                    pixels,
                    stride,
                ) {
                    failure = Some(error);
                }
            })?
        }
        Pixel::Mjpeg => {
            let channels = decode_mjpeg(data, layout.width, layout.height, jpeg)?;
            let decoded = &jpeg.pixels;
            pool.draw(&mut |pixels, stride| {
                if let Err(error) = rgb_to_bgra(decoded, channels, width, height, pixels, stride) {
                    failure = Some(error);
                }
            })?
        }
    };
    match failure {
        Some(error) => Err(error),
        None => Ok(frame),
    }
}

// ---------------------------------------------------------------------------
// Clock.

fn monotonic_nanos() -> i64 {
    let mut now: libc::timespec = zeroed();
    // SAFETY: a valid out-pointer for a clock every Linux kernel has.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    i64::try_from(i128::from(now.tv_sec) * 1_000_000_000 + i128::from(now.tv_nsec))
        .unwrap_or(i64::MAX)
}

/// The driver's capture time in CLOCK_MONOTONIC nanoseconds, if it gives
/// one (UVC does; older v4l2loopback copies the producer's).
fn buffer_nanos(buffer: &Buffer) -> Option<i64> {
    if buffer.flags & BUF_FLAG_TIMESTAMP_MASK != BUF_FLAG_TIMESTAMP_MONOTONIC {
        return None;
    }
    i64::try_from(
        i128::from(buffer.timestamp.tv_sec) * 1_000_000_000
            + i128::from(buffer.timestamp.tv_usec) * 1000,
    )
    .ok()
}

/// A picture's time on the engine clock. The engine clock is an `Instant`
/// (CLOCK_MONOTONIC on Linux) from an origin, so a driver timestamp maps
/// by its age at arrival. Implausible stamps fall back to arrival; times
/// always advance.
fn frame_time(
    captured: Option<i64>,
    arrived: Time,
    arrived_monotonic: i64,
    previous: Option<Time>,
) -> Time {
    let at = match captured {
        Some(nanos)
            if nanos > 0
                && nanos <= arrived_monotonic.saturating_add(MAX_TIMESTAMP_FUTURE_NS)
                && arrived_monotonic.saturating_sub(nanos) <= MAX_TIMESTAMP_AGE_NS =>
        {
            Time::from_nanos(
                arrived
                    .as_nanos()
                    .saturating_sub(arrived_monotonic.saturating_sub(nanos).max(0)),
            )
        }
        _ => arrived,
    };
    match previous {
        Some(previous) if at <= previous => Time::from_nanos(previous.as_nanos().saturating_add(1)),
        _ => at,
    }
}

// ---------------------------------------------------------------------------
// Streaming.

struct Mapping {
    pointer: *mut libc::c_void,
    len: usize,
}

/// A streaming device with its mmap'd driver buffers. Dropping it stops
/// the stream, unmaps the buffers and closes the device.
struct Stream {
    file: File,
    mappings: Vec<Mapping>,
    streaming: bool,
}

// SAFETY: the mappings are owned by the stream and only touched by the
// thread that owns it.
unsafe impl Send for Stream {}

impl Stream {
    fn start(file: File, minimum: usize) -> Result<Self> {
        let fd = file.as_raw_fd();
        let mut request: RequestBuffers = zeroed();
        request.count = BUFFERS;
        request.kind = BUF_TYPE_VIDEO_CAPTURE;
        request.memory = MEMORY_MMAP;
        xioctl(fd, VIDIOC_REQBUFS, &mut request).map_err(|error| {
            if error.raw_os_error() == Some(libc::EBUSY) {
                anyhow::anyhow!("camera.device: in use by another application")
            } else {
                anyhow::Error::new(error).context("camera.stream: VIDIOC_REQBUFS")
            }
        })?;
        ensure!(
            (2..=MAX_BUFFERS).contains(&request.count),
            "camera.stream: driver gave {} buffers; supported 2..={MAX_BUFFERS}",
            request.count
        );
        let mut stream = Self {
            file,
            mappings: Vec::with_capacity(request.count as usize),
            streaming: false,
        };
        for index in 0..request.count {
            let mut buffer: Buffer = zeroed();
            buffer.index = index;
            buffer.kind = BUF_TYPE_VIDEO_CAPTURE;
            buffer.memory = MEMORY_MMAP;
            xioctl(fd, VIDIOC_QUERYBUF, &mut buffer).context("camera.stream: VIDIOC_QUERYBUF")?;
            let len = buffer.length as usize;
            ensure!(
                (minimum.max(1)..=MAX_BUFFER_BYTES).contains(&len),
                "camera.stream: driver buffer of {len} bytes; needs {minimum}..={MAX_BUFFER_BYTES}"
            );
            // SAFETY: QUERYBUF on an MMAP buffer fills `offset`.
            let offset = unsafe { buffer.m.offset };
            // SAFETY: maps the driver's buffer `index` at the offset it
            // gave; unmapped in Drop.
            let pointer = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    offset as libc::off_t,
                )
            };
            if pointer == libc::MAP_FAILED {
                return Err(io::Error::last_os_error()).context("camera.stream: mmap");
            }
            stream.mappings.push(Mapping { pointer, len });
        }
        for index in 0..request.count {
            stream.queue(index).context("camera.stream: VIDIOC_QBUF")?;
        }
        let mut kind = BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        xioctl(fd, VIDIOC_STREAMON, &mut kind).context("camera.stream: VIDIOC_STREAMON")?;
        stream.streaming = true;
        Ok(stream)
    }

    fn queue(&self, index: u32) -> io::Result<()> {
        let mut buffer: Buffer = zeroed();
        buffer.index = index;
        buffer.kind = BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = MEMORY_MMAP;
        xioctl(self.file.as_raw_fd(), VIDIOC_QBUF, &mut buffer)
    }

    fn dequeue(&self) -> io::Result<Option<Buffer>> {
        let mut buffer: Buffer = zeroed();
        buffer.kind = BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = MEMORY_MMAP;
        match xioctl(self.file.as_raw_fd(), VIDIOC_DQBUF, &mut buffer) {
            Ok(()) => Ok(Some(buffer)),
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The picture a dequeued buffer holds, checked against its mapping.
    fn data(&self, buffer: &Buffer) -> Result<&[u8]> {
        let mapping = self
            .mappings
            .get(buffer.index as usize)
            .context("camera.stream: driver returned an unknown buffer index")?;
        let used = buffer.bytesused as usize;
        ensure!(
            used > 0 && used <= mapping.len,
            "camera.stream: driver reported {used} bytes in a {}-byte buffer",
            mapping.len
        );
        // SAFETY: the mapping is `mapping.len` bytes, alive while `self`
        // is, and the driver doesn't write a buffer while it is dequeued.
        Ok(unsafe { std::slice::from_raw_parts(mapping.pointer.cast::<u8>(), used) })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if self.streaming {
            let mut kind = BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
            let _ = xioctl(self.file.as_raw_fd(), VIDIOC_STREAMOFF, &mut kind);
        }
        for mapping in self.mappings.drain(..) {
            // SAFETY: mapped in `start` with this length; no slices
            // outlive the stream.
            unsafe { libc::munmap(mapping.pointer, mapping.len) };
        }
    }
}

struct Worker {
    stream: Stream,
    layout: Layout,
    pool: FramePool,
    jpeg: Jpeg,
    sink: VideoSink<Frame>,
    stop: Arc<AtomicBool>,
    previous: Option<Time>,
}

impl Worker {
    fn run(&mut self) -> Result<()> {
        let fd = self.stream.file.as_raw_fd();
        let mut last = Instant::now();
        let mut started = false;
        let mut bad = 0u32;
        let (mut pool_drops, mut bad_frames) = (0u64, 0u64);
        let mut reported = (0, 0);
        let mut report_at = Instant::now();
        while !self.stop.load(Ordering::Acquire) {
            if report_at.elapsed() >= Duration::from_secs(5) && reported != (pool_drops, bad_frames)
            {
                isocast_diagnostics::output::deferred(
                    "warn",
                    module_path!(),
                    format!(
                        "Linux camera discarded frames: pool={pool_drops}, invalid={bad_frames}"
                    ),
                );
                reported = (pool_drops, bad_frames);
                report_at = Instant::now();
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; bounded timeout so `stop` is seen.
            let ready = unsafe { libc::poll(&mut poll, 1, POLL_MS) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("camera.stream: poll");
            }
            if ready == 0 {
                let limit = if started {
                    FRAME_TIMEOUT
                } else {
                    FIRST_FRAME_TIMEOUT
                };
                ensure!(
                    last.elapsed() < limit,
                    "camera.stream: no picture for {} s",
                    limit.as_secs()
                );
                continue;
            }
            let buffer = match self.stream.dequeue() {
                Ok(Some(buffer)) => buffer,
                Ok(None) => {
                    ensure!(
                        poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0,
                        "camera.stream: the device reported an error or was disconnected"
                    );
                    continue;
                }
                Err(error) if error.raw_os_error() == Some(libc::ENODEV) => {
                    bail!("camera.stream: the camera was disconnected")
                }
                // Failed DQBUF provides no trustworthy index to requeue.
                // Never guess: repeated EIO is bounded by MAX_BAD_FRAMES.
                // If a driver consumes a buffer despite the error, eventual
                // starvation fails via poll or the no-picture timeout. Drop
                // then STREAMOFFs; the engine reopens and queues all buffers.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                    bad += 1;
                    bad_frames += 1;
                    ensure!(
                        bad < MAX_BAD_FRAMES,
                        "camera.stream: {bad} consecutive device I/O errors (VIDIOC_DQBUF: {error})"
                    );
                    continue;
                }
                Err(error) => return Err(error).context("camera.stream: VIDIOC_DQBUF"),
            };
            let arrived = system::now();
            let arrived_monotonic = monotonic_nanos();
            last = Instant::now();
            let frame = if buffer.flags & BUF_FLAG_ERROR != 0 {
                Err(anyhow::anyhow!(
                    "camera.stream: the driver marked a picture corrupt"
                ))
            } else {
                self.stream
                    .data(&buffer)
                    .and_then(|data| draw_frame(&mut self.pool, &self.layout, data, &mut self.jpeg))
            };
            // Converted (or dropped): the driver may refill it.
            self.stream
                .queue(buffer.index)
                .context("camera.stream: VIDIOC_QBUF")?;
            match frame {
                Ok(Some(frame)) => {
                    started = true;
                    bad = 0;
                    let at = frame_time(
                        buffer_nanos(&buffer),
                        arrived,
                        arrived_monotonic,
                        self.previous,
                    );
                    self.previous = Some(at);
                    (self.sink.frame)(frame, at);
                }
                // Every pooled frame is still downstream: drop this one.
                Ok(None) => pool_drops += 1,
                Err(error) => {
                    bad += 1;
                    bad_frames += 1;
                    if bad >= MAX_BAD_FRAMES {
                        return Err(error.context(format!(
                            "camera.stream: {MAX_BAD_FRAMES} unusable pictures in a row"
                        )));
                    }
                }
            }
        }
        if reported != (pool_drops, bad_frames) {
            isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!("Linux camera discarded frames: pool={pool_drops}, invalid={bad_frames}"),
            );
        }
        Ok(())
    }
}

/// A running camera capture. Dropping it stops the stream within about
/// [`POLL_MS`], waiting at most [`STOP_TIMEOUT`] for the thread.
pub struct Capture {
    stop: Arc<AtomicBool>,
    exited: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
    name: String,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The thread holds the sender; it disconnects when the thread ends.
        match self.exited.recv_timeout(STOP_TIMEOUT) {
            Err(mpsc::RecvTimeoutError::Timeout) => isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!(
                    "Linux camera {:?}: capture thread did not stop within {} s; detached",
                    self.name,
                    STOP_TIMEOUT.as_secs()
                ),
            ),
            _ => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
    }
}

fn set_format(fd: RawFd, mode: &Mode) -> Result<Layout> {
    let mut format: Format = zeroed();
    format.kind = BUF_TYPE_VIDEO_CAPTURE;
    format.fmt.pix = PixFormat {
        width: mode.width,
        height: mode.height,
        pixelformat: mode.fourcc,
        field: FIELD_NONE,
        ..zeroed()
    };
    xioctl(fd, VIDIOC_S_FMT, &mut format).map_err(|error| {
        if error.raw_os_error() == Some(libc::EBUSY) {
            anyhow::anyhow!("camera.device: in use by another application")
        } else {
            anyhow::Error::new(error).context("camera.format: VIDIOC_S_FMT")
        }
    })?;
    // SAFETY: a capture-type S_FMT returns the `pix` member.
    let pix = unsafe { format.fmt.pix };
    ensure!(
        (pix.width, pix.height, pix.pixelformat) == (mode.width, mode.height, mode.fourcc),
        "camera.format: asked for {}x{} {}, the driver chose {}x{} {}",
        mode.width,
        mode.height,
        fourcc_name(mode.fourcc),
        pix.width,
        pix.height,
        fourcc_name(pix.pixelformat)
    );
    ensure!(
        matches!(pix.field, FIELD_ANY | FIELD_NONE),
        "camera.format: interlaced pictures (field {}) are not supported",
        pix.field
    );
    let row = match mode.pixel {
        Pixel::Yuyv => Some(mode.width as usize * 2),
        Pixel::Nv12 => Some(mode.width as usize),
        Pixel::Mjpeg => None,
    };
    let stride = match row {
        None => 0,
        Some(row) => {
            let stride = if pix.bytesperline == 0 {
                row
            } else {
                pix.bytesperline as usize
            };
            ensure!(
                (row..=MAX_DIMENSION as usize * 4).contains(&stride),
                "camera.format: {} row stride {stride} for width {}",
                fourcc_name(mode.fourcc),
                mode.width
            );
            stride
        }
    };
    Ok(Layout {
        pixel: mode.pixel,
        width: mode.width,
        height: mode.height,
        stride,
        yuv: Yuv::from_format(pix.colorspace, pix.ycbcr_enc, pix.quantization),
    })
}

/// Sets the mode's frame interval where the driver allows it and returns
/// the nominal rate the device runs at.
fn set_rate(fd: RawFd, mode: &Mode, request: CameraRequest, canvas: &Canvas) -> Result<u32> {
    let mut actual = None;
    if let Some(parm) = current_parm(fd) {
        actual = Some(parm.timeperframe);
        let interval = mode.interval.or_else(|| {
            request.fps.map(|fps| Fract {
                numerator: 1,
                denominator: fps,
            })
        });
        if parm.capability & CAP_TIMEPERFRAME != 0
            && let Some(interval) = interval
        {
            let mut set: StreamParm = zeroed();
            set.kind = BUF_TYPE_VIDEO_CAPTURE;
            set.parm.capture = CaptureParm {
                timeperframe: interval,
                ..parm
            };
            xioctl(fd, VIDIOC_S_PARM, &mut set).context("camera.fps: VIDIOC_S_PARM")?;
            // SAFETY: a capture-type S_PARM returns the `capture` member.
            actual = Some(unsafe { set.parm.capture.timeperframe });
        }
    }
    let actual = actual.and_then(nominal);
    ensure!(
        request.fps.is_none() || actual.or(mode.fps).is_some(),
        "camera.fps: driver does not report a verifiable rate; omit fps to record at the canvas rate"
    );
    if let (Some(want), Some(have)) = (request.fps, actual) {
        ensure!(
            want == have,
            "camera.fps: asked for {want}, the driver runs this mode at {have}"
        );
    }
    Ok(actual
        .or(mode.fps)
        .unwrap_or(request.fps.unwrap_or(canvas.fps)))
}

/// Start capturing `device` (an id, a `/dev/video` node, a name or part
/// of one) in the mode `request` asks for, or the nearest to `canvas`.
pub fn capture(
    device: &str,
    request: CameraRequest,
    canvas: &Canvas,
    sink: VideoSink<Frame>,
) -> Result<(Capture, CaptureFormat)> {
    if request.fps.is_some_and(|fps| !(1..=240).contains(&fps)) {
        bail!("camera.fps: valid integer range is 1..=240");
    }
    if let Some((width, height)) = request.size {
        ensure!(
            (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height),
            "camera.size: {width}x{height}; valid sizes are 1..=8192 each way"
        );
    }
    let (identity, file) = resolve(device)?;
    ensure!(
        identity.caps & CAP_STREAMING != 0,
        "camera.device: {} does not support streaming",
        identity.camera.name
    );
    let fd = file.as_raw_fd();
    let sizes = sizes(fd)?;
    let mode = select(&modes(&sizes), request, canvas)
        .with_context(|| format!("camera {:?} ({})", identity.camera.name, identity.camera.id))?;
    // Not while the webcam's own microphone sets up (usb_setup).
    let setup = usb_setup::begin(usb_setup::video_node_device(&identity.node));
    let layout = set_format(fd, &mode)?;
    let fps = set_rate(fd, &mode, request, canvas)?;
    let (width, height) = (layout.width as usize, layout.height as usize);
    let minimum = match layout.pixel {
        Pixel::Yuyv => extent(layout.stride, width * 2, height),
        Pixel::Nv12 => nv12_extent(layout.stride, width, height),
        Pixel::Mjpeg => Some(1),
    }
    .context("camera.format: frame size overflow")?;
    let pool = FramePool::new(layout.width, layout.height)?;
    let stream = Stream::start(file, minimum)?;
    drop(setup);
    let stop = Arc::new(AtomicBool::new(false));
    let (exited, receiver) = mpsc::channel::<()>();
    let mut worker = Worker {
        stream,
        layout,
        pool,
        jpeg: Jpeg::default(),
        sink,
        stop: stop.clone(),
        previous: None,
    };
    let name = identity.camera.name.clone();
    let thread = std::thread::Builder::new()
        .name("v4l2 camera".into())
        .spawn(move || {
            let _exited = exited;
            let result = catch_unwind(AssertUnwindSafe(|| worker.run()))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("camera.stream: capture thread panicked")));
            if let Err(error) = result
                && !worker.stop.load(Ordering::Acquire)
            {
                (worker.sink.stopped)(format!("{error:#}"));
            }
            // Stop streaming and unmap before the owner's join returns.
            drop(worker);
        })
        .context("starting the camera thread")?;
    Ok((
        Capture {
            stop,
            exited: receiver,
            thread: Some(thread),
            name,
        },
        CaptureFormat {
            width: layout.width,
            height: layout.height,
            fps,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2)
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn ioctl_numbers_match_the_kernel_abi() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(fourcc_name(FOURCC_MJPG), "MJPG");
        assert_eq!(fourcc_name(FOURCC_YUYV), "YUYV");
        assert_eq!(fourcc_name(FOURCC_NV12), "NV12");
    }

    #[test]
    fn yuyv_limited_bt601_converts_reference_colors() {
        let yuv = Yuv::from_format(8, 0, 0);
        assert_eq!(yuv, Yuv::new(Yuv::BT601, false));
        assert!(close(yuv.bgra(235, 128, 128), [255, 255, 255, 255]));
        assert!(close(yuv.bgra(16, 128, 128), [0, 0, 0, 255]));
        assert!(close(yuv.bgra(126, 128, 128), [128, 128, 128, 255]));
        assert!(close(yuv.bgra(81, 90, 240), [0, 0, 255, 255]));
        assert!(close(yuv.bgra(145, 54, 34), [0, 255, 0, 255]));
        assert!(close(yuv.bgra(41, 240, 110), [255, 0, 0, 255]));
        // Out-of-range input clamps instead of wrapping.
        assert_eq!(yuv.bgra(255, 255, 255)[2], 255);
        assert_eq!(yuv.bgra(0, 0, 0)[0], 0);
    }

    #[test]
    fn yuyv_full_range_and_bt709_follow_the_driver() {
        let full = Yuv::from_format(7, 0, 0);
        assert_eq!(full, Yuv::new(Yuv::BT601, true));
        assert!(close(full.bgra(255, 128, 128), [255, 255, 255, 255]));
        assert!(close(full.bgra(0, 128, 128), [0, 0, 0, 255]));
        let hd = Yuv::from_format(3, 0, 2);
        assert_eq!(hd, Yuv::new(Yuv::BT709, false));
        // Rec. 709 limited red: Y 63, Cb 102, Cr 240.
        assert!(close(hd.bgra(63, 102, 240), [0, 0, 255, 255]));
    }

    #[test]
    fn yuyv_respects_strides_and_rejects_short_frames() {
        let yuv = Yuv::new(Yuv::BT601, false);
        // 4x2, source stride 12 (4 padding bytes), target stride 20.
        let mut source = vec![0xee; 12 * 2];
        source[..8].copy_from_slice(&[235, 128, 235, 128, 16, 128, 16, 128]);
        source[12..20].copy_from_slice(&[81, 90, 81, 240, 126, 128, 126, 128]);
        let mut target = vec![7; 20 * 2];
        yuyv_to_bgra(&source, 12, 4, 2, &yuv, &mut target, 20).unwrap();
        assert!(close(
            target[0..4].try_into().unwrap(),
            [255, 255, 255, 255]
        ));
        assert!(close(target[12..16].try_into().unwrap(), [0, 0, 0, 255]));
        assert!(close(target[20..24].try_into().unwrap(), [0, 0, 255, 255]));
        assert!(close(
            target[28..32].try_into().unwrap(),
            [128, 128, 128, 255]
        ));
        // Target padding is untouched.
        assert_eq!(&target[16..20], &[7, 7, 7, 7]);
        // The last row needs no padding; one byte less is refused.
        let short = &source[..12 + 8 - 1];
        assert!(yuyv_to_bgra(short, 12, 4, 2, &yuv, &mut target, 20).is_err());
        assert!(yuyv_to_bgra(&source, 4, 4, 2, &yuv, &mut target, 20).is_err());
        assert!(yuyv_to_bgra(&source, 12, 3, 2, &yuv, &mut target, 20).is_err());
        assert!(yuyv_to_bgra(&source, 12, 4, 2, &yuv, &mut target[..30], 20).is_err());
    }

    /// BGRA pixel `(x, y)` of a converted picture.
    fn pixel(target: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        target[y * stride + x * 4..][..4].try_into().unwrap()
    }

    #[test]
    fn nv12_chroma_rows_cover_two_luma_rows() {
        let yuv = Yuv::new(Yuv::BT601, false);
        // 2x4: red chroma over blue chroma.
        let source = [81, 81, 81, 81, 41, 41, 41, 41, 90, 240, 240, 110];
        let mut target = [0; 32];
        nv12_to_bgra(&source, 2, 2, 4, &yuv, &mut target, 8).unwrap();
        let (red, blue) = ([0, 0, 255, 255], [255, 0, 0, 255]);
        for (y, want) in [(0, red), (1, red), (2, blue), (3, blue)] {
            assert!(close(pixel(&target, 8, 1, y), want), "row {y}");
        }
    }

    #[test]
    fn nv12_respects_odd_strides_and_rejects_short_frames() {
        let yuv = Yuv::new(Yuv::BT601, false);
        // 4x2 at an odd source stride of 7 (3 padding bytes per row); the
        // chroma row starts at 7 * 2, after the luma rows' padding.
        let mut source = vec![0xee; 7 * 2 + 4];
        source[..4].copy_from_slice(&[235, 16, 81, 81]);
        source[7..11].copy_from_slice(&[16, 235, 81, 81]);
        source[14..18].copy_from_slice(&[128, 128, 90, 240]);
        let mut target = vec![7; 20 * 2];
        nv12_to_bgra(&source, 7, 4, 2, &yuv, &mut target, 20).unwrap();
        assert!(close(pixel(&target, 20, 0, 0), [255, 255, 255, 255]));
        assert!(close(pixel(&target, 20, 1, 0), [0, 0, 0, 255]));
        assert!(close(pixel(&target, 20, 3, 0), [0, 0, 255, 255]));
        assert!(close(pixel(&target, 20, 0, 1), [0, 0, 0, 255]));
        // Target padding is untouched.
        assert_eq!(&target[16..20], &[7, 7, 7, 7]);

        // The last chroma row needs no padding; one byte less is refused.
        assert!(nv12_to_bgra(&source[..17], 7, 4, 2, &yuv, &mut target, 20).is_err());
        // A stride shorter than a row, odd and empty sizes and a short
        // target are errors, never panics.
        assert!(nv12_to_bgra(&source, 3, 4, 2, &yuv, &mut target, 20).is_err());
        assert!(nv12_to_bgra(&source, 7, 3, 2, &yuv, &mut target, 20).is_err());
        assert!(nv12_to_bgra(&source, 7, 4, 1, &yuv, &mut target, 20).is_err());
        assert!(nv12_to_bgra(&source, 7, 0, 2, &yuv, &mut target, 20).is_err());
        assert!(nv12_to_bgra(&source, 7, 4, 2, &yuv, &mut target, 15).is_err());
        assert!(nv12_to_bgra(&source, 7, 4, 2, &yuv, &mut target[..35], 20).is_err());
        assert_eq!(nv12_extent(usize::MAX, 4, 2), None);
    }

    #[test]
    fn nv12_frames_come_from_the_pool() {
        let mut pool = FramePool::new(2, 2).unwrap();
        let layout = Layout {
            pixel: Pixel::Nv12,
            width: 2,
            height: 2,
            stride: 2,
            yuv: Yuv::new(Yuv::BT601, false),
        };
        let mut jpeg = Jpeg::default();
        let data = [235, 16, 16, 235, 128, 128];
        let frame = draw_frame(&mut pool, &layout, &data, &mut jpeg)
            .unwrap()
            .unwrap();
        let bgra = frame.read_bgra().unwrap();
        assert!(close(bgra[4..8].try_into().unwrap(), [0, 0, 0, 255]));
        assert!(draw_frame(&mut pool, &layout, &data[..5], &mut jpeg).is_err());
    }

    fn encode(width: u32, height: u32) -> (Vec<u8>, image::RgbImage) {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 8) as u8, 200])
        });
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95)
            .encode(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        (bytes, image)
    }

    /// The same JPEG without its DHT segments, like many UVC cameras send.
    fn strip_huffman(data: &[u8]) -> Vec<u8> {
        let mut out = data[..2].to_vec();
        let mut at = 2;
        loop {
            let marker = data[at + 1];
            if marker == 0xda {
                out.extend_from_slice(&data[at..]);
                return out;
            }
            let length = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
            if marker != 0xc4 {
                out.extend_from_slice(&data[at..at + 2 + length]);
            }
            at += 2 + length;
        }
    }

    #[test]
    fn mjpeg_decodes_to_bgra_with_and_without_huffman_tables() {
        let (bytes, image) = encode(64, 32);
        let mut jpeg = Jpeg::default();
        assert_eq!(missing_huffman(&bytes).unwrap(), None);
        assert_eq!(decode_mjpeg(&bytes, 64, 32, &mut jpeg).unwrap(), 3);
        let with_tables = jpeg.pixels.clone();
        for (decoded, original) in with_tables.as_chunks::<3>().0.iter().zip(image.pixels()) {
            assert!(
                decoded
                    .iter()
                    .zip(original.0)
                    .all(|(a, b)| a.abs_diff(b) <= 12)
            );
        }

        let stripped = strip_huffman(&bytes);
        assert!(stripped.len() < bytes.len());
        assert!(missing_huffman(&stripped).unwrap().is_some());
        assert_eq!(decode_mjpeg(&stripped, 64, 32, &mut jpeg).unwrap(), 3);
        assert_eq!(
            jpeg.pixels, with_tables,
            "Annex K tables match the encoder's"
        );

        let mut pool = FramePool::new(64, 32).unwrap();
        let layout = Layout {
            pixel: Pixel::Mjpeg,
            width: 64,
            height: 32,
            stride: 0,
            yuv: Yuv::new(Yuv::BT601, true),
        };
        let frame = draw_frame(&mut pool, &layout, &stripped, &mut jpeg)
            .unwrap()
            .unwrap();
        let bgra = frame.read_bgra().unwrap();
        assert_eq!(bgra.len(), 64 * 32 * 4);
        let rgb = &with_tables[..3];
        assert_eq!(&bgra[..4], &[rgb[2], rgb[1], rgb[0], 255]);
    }

    #[test]
    fn huffman_tables_have_the_counts_their_lengths_declare() {
        for (bits, values) in [
            (&DC_LUMA_BITS, &DC_VALUES[..]),
            (&DC_CHROMA_BITS, &DC_VALUES[..]),
            (&AC_LUMA_BITS, &AC_LUMA_VALUES[..]),
            (&AC_CHROMA_BITS, &AC_CHROMA_VALUES[..]),
        ] {
            assert_eq!(
                bits.iter().map(|b| usize::from(*b)).sum::<usize>(),
                values.len()
            );
        }
        let segment = default_huffman_segment();
        assert_eq!(segment.len(), 2 + 418);
        assert_eq!(&segment[..4], &[0xff, 0xc4, 0x01, 0xa2]);
    }

    #[test]
    fn corrupt_mjpeg_is_an_error_never_a_panic() {
        let (bytes, _) = encode(32, 16);
        let mut jpeg = Jpeg::default();
        assert!(
            decode_mjpeg(&bytes, 64, 16, &mut jpeg).is_err(),
            "size mismatch"
        );
        assert!(decode_mjpeg(&[], 32, 16, &mut jpeg).is_err());
        assert!(decode_mjpeg(&[0xff, 0xd8, 0xff], 32, 16, &mut jpeg).is_err());
        assert!(decode_mjpeg(&[0x00; 64], 32, 16, &mut jpeg).is_err());
        for cut in [3, 10, bytes.len() / 3, bytes.len() / 2] {
            let _ = decode_mjpeg(&bytes[..cut], 32, 16, &mut jpeg);
        }
        let mut bad_length = bytes.clone();
        bad_length[4] = 0xff;
        bad_length[5] = 0xff;
        assert!(decode_mjpeg(&bad_length, 32, 16, &mut jpeg).is_err());
        let mut noise = bytes.clone();
        let half = noise.len() / 2;
        for (index, byte) in noise[half..].iter_mut().enumerate() {
            *byte ^= (index * 37) as u8;
        }
        let _ = decode_mjpeg(&noise, 32, 16, &mut jpeg);
    }

    #[test]
    fn yuyv_frames_come_from_the_bounded_pool() {
        let mut pool = FramePool::new(2, 1).unwrap();
        let layout = Layout {
            pixel: Pixel::Yuyv,
            width: 2,
            height: 1,
            stride: 4,
            yuv: Yuv::new(Yuv::BT601, false),
        };
        let mut jpeg = Jpeg::default();
        let frame = draw_frame(&mut pool, &layout, &[235, 128, 16, 128], &mut jpeg)
            .unwrap()
            .unwrap();
        let bgra = frame.read_bgra().unwrap();
        assert!(close(bgra[..4].try_into().unwrap(), [255, 255, 255, 255]));
        assert!(close(bgra[4..].try_into().unwrap(), [0, 0, 0, 255]));
        assert!(draw_frame(&mut pool, &layout, &[235, 128], &mut jpeg).is_err());
        let held: Vec<_> = std::iter::from_fn(|| {
            draw_frame(&mut pool, &layout, &[235, 128, 16, 128], &mut jpeg).unwrap()
        })
        .take(64)
        .collect();
        assert!(held.len() < 64, "pool is bounded");
        assert!(
            draw_frame(&mut pool, &layout, &[235, 128, 16, 128], &mut jpeg)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn camera_access_needs_one_node_that_opens() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("isocast-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let node = |name: &str, mode: u32| {
            let path = dir.join(name);
            std::fs::write(&path, b"").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        };
        let closed = node("video0", 0o000);
        let open = node("video1", 0o600);

        assert_eq!(access_to(&[]), None);
        assert_eq!(access_to(&[closed.clone(), open]), Some(true));
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(access_to(&[closed]), Some(false));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nominal_rates() {
        let f = |numerator, denominator| {
            nominal(Fract {
                numerator,
                denominator,
            })
        };
        assert_eq!(f(1, 30), Some(30));
        assert_eq!(f(1001, 30000), Some(30));
        assert_eq!(f(1001, 60000), Some(60));
        assert_eq!(f(333_333, 10_000_000), Some(30));
        assert_eq!(f(2, 15), None, "7.5 fps has no integer rate");
        assert_eq!(f(0, 30), None);
        assert_eq!(f(1, 0), None);
        assert_eq!(f(1, 1000), None);
        assert_eq!(f(u32::MAX, u32::MAX), Some(1));
    }

    #[test]
    fn stepwise_sizes_respect_range_and_step() {
        let sizes = stepwise_sizes([16, 1920, 16, 16, 1080, 8]);
        assert!(sizes.contains(&(1920, 1080)));
        assert!(sizes.contains(&(1280, 720)));
        assert!(sizes.contains(&(16, 16)));
        assert!(!sizes.contains(&(3840, 2160)));
        // 1080 - 16 is not a multiple of 16: no 1080-high rows.
        assert!(
            stepwise_sizes([16, 1920, 16, 16, 1080, 16])
                .iter()
                .all(|(_, h)| (h - 16).is_multiple_of(16))
        );
        assert!(
            stepwise_sizes([1, 100_000, 1, 1, 100_000, 1])
                .iter()
                .all(|(w, h)| *w <= MAX_DIMENSION && *h <= MAX_DIMENSION)
        );
    }

    fn size(code: u32, width: u32, height: u32, rates: &[u32]) -> Size {
        Size {
            fourcc: code,
            width,
            height,
            rates: Rates::Discrete(
                rates
                    .iter()
                    .map(|&fps| Fract {
                        numerator: 1,
                        denominator: fps,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn a_node_with_no_decodable_format_is_not_a_second_camera() {
        let node = |n: u32, name: &str| Identity {
            node: PathBuf::from(format!("/dev/video{n}")),
            caps: CAP_VIDEO_CAPTURE | CAP_STREAMING,
            camera: Camera {
                id: format!("/dev/v4l/by-id/usb-046d_Logitech_BRIO-video-index{n}"),
                name: name.into(),
                model: String::new(),
                manufacturer: String::new(),
                kind: CameraKind::External,
                formats: Vec::new(),
                suspended: false,
            },
        };
        // A BRIO's colour and infrared nodes, and a node with unreadable formats.
        let nodes = || {
            [
                node(0, "Logitech BRIO"),
                node(2, "Logitech BRIO"),
                node(4, "Capture card"),
            ]
        };
        let sizes = [
            Ok(vec![size(FOURCC_YUYV, 1920, 1080, &[30])]),
            Ok(Vec::new()),
            Err(anyhow::anyhow!("camera.formats: VIDIOC_ENUM_FMT: EIO")),
        ];
        // Unfiltered, the name is ambiguous.
        assert_eq!(pick(&nodes(), "Logitech BRIO"), None);

        let cameras: Vec<Identity> = nodes()
            .into_iter()
            .zip(&sizes)
            .filter(|(_, sizes)| is_camera(sizes))
            .map(|(identity, _)| identity)
            .collect();
        let listed: Vec<_> = cameras.iter().map(|c| c.node.to_str().unwrap()).collect();
        assert_eq!(listed, ["/dev/video0", "/dev/video4"]);
        assert_eq!(pick(&cameras, "Logitech BRIO"), Some(0));
        assert_eq!(pick(&cameras, "brio"), Some(0));
        assert_eq!(pick(&cameras, "/dev/video2"), None);
        assert_eq!(pick(&cameras, "capture"), Some(1));
    }

    #[test]
    fn an_nv12_only_camera_is_listed_and_selected() {
        assert_eq!(Pixel::from_fourcc(FOURCC_NV12), Some(Pixel::Nv12));
        assert!(Pixel::Nv12.fits(1920, 1080));
        assert!(!Pixel::Nv12.fits(1921, 1080), "odd width");
        assert!(!Pixel::Nv12.fits(1920, 1081), "odd height");
        assert!(Pixel::Yuyv.fits(1920, 1081));

        let sizes = [size(FOURCC_NV12, 1920, 1080, &[30])];
        assert_eq!(video_formats(&sizes)[0].pixel_format, "NV12");
        let canvas = Canvas {
            width: 1920,
            height: 1080,
            fps: 30,
        };
        let mode = select(&modes(&sizes), CameraRequest::default(), &canvas).unwrap();
        assert_eq!((mode.width, mode.pixel), (1920, Pixel::Nv12));
    }

    #[test]
    fn selection_prefers_canvas_size_then_rate_then_yuyv() {
        let sizes = [
            size(FOURCC_YUYV, 1920, 1080, &[5]),
            size(FOURCC_YUYV, 640, 480, &[30, 15]),
            size(FOURCC_MJPG, 1920, 1080, &[30, 60]),
            size(FOURCC_MJPG, 640, 480, &[30]),
            size(FOURCC_NV12, 1920, 1080, &[30]),
            size(FOURCC_NV12, 640, 480, &[30]),
        ];
        let modes = modes(&sizes);
        let canvas = Canvas {
            width: 1920,
            height: 1080,
            fps: 30,
        };
        let pick = |request| select(&modes, request, &canvas).unwrap();
        // NV12 before MJPEG, YUYV before NV12.
        let mode = pick(CameraRequest::default());
        assert_eq!(
            (mode.width, mode.fps, mode.pixel),
            (1920, Some(30), Pixel::Nv12)
        );
        let mode = pick(CameraRequest {
            size: Some((640, 480)),
            fps: None,
        });
        assert_eq!(
            (mode.width, mode.fps, mode.pixel),
            (640, Some(30), Pixel::Yuyv)
        );
        let mode = pick(CameraRequest {
            size: None,
            fps: Some(5),
        });
        assert_eq!((mode.width, mode.pixel), (1920, Pixel::Yuyv));
        let error = select(
            &modes,
            CameraRequest {
                size: Some((1280, 720)),
                fps: None,
            },
            &canvas,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("camera.size/fps") && error.contains("1920x1080@60 MJPG"));
    }

    #[test]
    fn ranges_and_unknown_rates_still_give_modes() {
        let sizes = [
            Size {
                fourcc: FOURCC_YUYV,
                width: 1280,
                height: 720,
                rates: Rates::Range {
                    fastest: Fract {
                        numerator: 1,
                        denominator: 60,
                    },
                    slowest: Fract {
                        numerator: 1,
                        denominator: 10,
                    },
                },
            },
            Size {
                fourcc: FOURCC_MJPG,
                width: 640,
                height: 480,
                rates: Rates::Unknown,
            },
        ];
        let modes = modes(&sizes);
        let rates: Vec<_> = modes.iter().filter_map(|m| m.fps).collect();
        assert_eq!(rates, vec![10, 15, 20, 24, 25, 30, 48, 50, 60]);
        assert!(modes.iter().any(|m| m.fps.is_none() && m.width == 640));
        let formats = video_formats(&sizes);
        assert_eq!(formats[0].width, 1280);
        assert_eq!(formats[0].frame_rates, vec![10.0, 60.0]);
        assert_eq!(
            formats[0].frame_rate_ranges,
            vec![FrameRateRange {
                min: 10.0,
                max: 60.0
            }]
        );
        assert_eq!(formats[1].pixel_format, "MJPG");
        assert!(formats[1].frame_rates.is_empty());
    }

    #[test]
    fn frame_times_map_driver_stamps_onto_the_engine_clock() {
        let arrived = Time::from_nanos(5_000_000_000);
        let mono = 100_000_000_000;
        // Captured 20 ms before it arrived.
        assert_eq!(
            frame_time(Some(mono - 20_000_000), arrived, mono, None),
            Time::from_nanos(4_980_000_000)
        );
        // Absent, ancient or future stamps fall back to arrival.
        assert_eq!(frame_time(None, arrived, mono, None), arrived);
        assert_eq!(frame_time(Some(1), arrived, mono, None), arrived);
        assert_eq!(
            frame_time(Some(mono + 50_000_000), arrived, mono, None),
            arrived
        );
        assert_eq!(frame_time(Some(-5), arrived, mono, None), arrived);
        // Never runs backwards.
        assert_eq!(
            frame_time(Some(mono - 20_000_000), arrived, mono, Some(arrived)),
            Time::from_nanos(5_000_000_001)
        );
    }
}
