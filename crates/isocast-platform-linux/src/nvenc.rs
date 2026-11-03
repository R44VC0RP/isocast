//! Synchronous Linux NVENC H.264 using the system NVIDIA driver.
//!
//! One input and one output are retained per encoder. Native completion blocks
//! the calling writer/live-encoder worker, just like the software backend; no
//! detached worker or timeout pretends to cancel a driver call. BGRA readback
//! and Rec.709 conversion remain on the CPU in this first hardware path.

use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use isocast_engine::{EncodedVideo, LiveVideoSpec, ParameterSets, Time, VideoEncoder};
use isocast_protocol::api::H264Profile;

use crate::encoder::avcc;
use crate::frames::Frame;

const MAX_PACKET: usize = 16 * 1024 * 1024;
const ERROR_CAPACITY: usize = 512;
// A failed initial session probe can be transient (for example another app
// holds every session). A later successful open supersedes that verdict,
// without probing again while our own encoders may hold the session budget.
static SESSION_OPENED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn ic_nvenc_open(
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
        gop: u32,
        qp: u32,
        cbr: u32,
        profile: u32,
        error: *mut c_char,
        error_cap: usize,
    ) -> *mut c_void;
    fn ic_nvenc_headers(
        encoder: *mut c_void,
        out: *mut u8,
        capacity: usize,
        written: *mut usize,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn ic_nvenc_staging(encoder: *mut c_void, size: *mut usize, pitch: *mut usize) -> *mut u8;
    fn ic_nvenc_encode(
        encoder: *mut c_void,
        pts: i64,
        force_idr: u32,
        out: *mut u8,
        capacity: usize,
        written: *mut usize,
        output_pts: *mut i64,
        keyframe: *mut u32,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn ic_nvenc_set_rate(
        encoder: *mut c_void,
        kbps: u32,
        fps: u32,
        error: *mut c_char,
        error_cap: usize,
    ) -> i32;
    fn ic_nvenc_close(encoder: *mut c_void);
}

struct Native(NonNull<c_void>);

// SAFETY: the bridge detaches its CUDA context between calls and pushes it on
// each caller thread. Borrowing staging requires exclusive access to the owner,
// so it cannot move or submit work while borrowed. NonNull keeps it !Sync.
unsafe impl Send for Native {}

impl Native {
    fn staging(&mut self) -> Result<(&mut [u8], usize)> {
        let (mut size, mut pitch) = (0, 0);
        // SAFETY: live exclusive session and writable scalar outputs.
        let ptr = unsafe { ic_nvenc_staging(self.0.as_ptr(), &mut size, &mut pitch) };
        ensure!(
            !ptr.is_null(),
            "NVENC: staging unavailable after encoder failure"
        );
        // SAFETY: the bridge owns one initialized `size`-byte host allocation
        // until close. No driver use overlaps this borrow: upload is synchronous.
        Ok((unsafe { std::slice::from_raw_parts_mut(ptr, size) }, pitch))
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the live native session.
        unsafe { ic_nvenc_close(self.0.as_ptr()) };
    }
}

fn error_message(error: &[c_char; ERROR_CAPACITY]) -> String {
    // The last byte remains zero even if the bridge fills the rest.
    // SAFETY: the zero-initialized fixed array is always NUL-terminated.
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub struct Encoder {
    native: Native,
    spec: LiveVideoSpec,
    bitstream: Vec<u8>,
    parameters: ParameterSets,
    ready: Option<EncodedVideo>,
    last: Option<Time>,
    dropped: u64,
    created_kbps: u32,
    recording: bool,
    rate_uncertain: bool,
}

impl Encoder {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        Self::configured(spec, None)
    }

    pub(crate) fn recording(width: u32, height: u32, fps: u32, quality: u8) -> Result<Self> {
        ensure!(
            (1..=100).contains(&quality),
            "video.encoder.quality: valid range is 1..=100"
        );
        Self::configured(
            &LiveVideoSpec {
                profile: H264Profile::High,
                width,
                height,
                fps,
                bitrate_kbps: 100_000,
                constant_bitrate: false,
                keyframe_interval: fps.saturating_mul(2),
                low_latency: true,
            },
            Some(38 - u32::from(quality) * 22 / 100),
        )
    }

    fn configured(spec: &LiveVideoSpec, qp: Option<u32>) -> Result<Self> {
        ensure!(
            spec.width >= 16
                && spec.height >= 16
                && spec.width.is_multiple_of(2)
                && spec.height.is_multiple_of(2)
                && spec.width.max(spec.height) <= 3840
                && spec.width.min(spec.height) <= 2160,
            "video.width/height: NVENC requires even dimensions from 16 up to 3840x2160 (or portrait)"
        );
        ensure!(
            (1..=60).contains(&spec.fps)
                && (1..=100_000).contains(&spec.bitrate_kbps)
                && spec.keyframe_interval > 0,
            "video: fps must be 1..=60, bitrate_kbps 1..=100000 and keyframe_interval positive"
        );
        let mut error = [0; ERROR_CAPACITY];
        // SAFETY: scalar settings were bounded above; error is writable and
        // the bridge returns either a uniquely owned session or null.
        let raw = unsafe {
            ic_nvenc_open(
                spec.width,
                spec.height,
                spec.fps,
                spec.bitrate_kbps,
                spec.keyframe_interval,
                qp.unwrap_or(u32::MAX),
                u32::from(spec.constant_bitrate),
                match spec.profile {
                    H264Profile::High => 0,
                    H264Profile::ConstrainedBaseline => 1,
                },
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let native = Native(NonNull::new(raw).with_context(|| error_message(&error))?);
        let mut headers = vec![0; 64 * 1024];
        let mut length = 0;
        // SAFETY: native is live and output/length/error have the given capacity.
        let result = unsafe {
            ic_nvenc_headers(
                native.0.as_ptr(),
                headers.as_mut_ptr(),
                headers.len(),
                &mut length,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(result == 0, "{}", error_message(&error));
        let headers = headers
            .get(..length)
            .context("NVENC: invalid header length")?;
        let parameters = parameters(&nals(headers)?);
        avcc(&parameters)?;
        ensure!(
            parameters.sps.iter().all(|sps| match spec.profile {
                H264Profile::High => sps.get(1) == Some(&100),
                H264Profile::ConstrainedBaseline => {
                    sps.get(1) == Some(&66) && sps.get(2).is_some_and(|f| f & 0x40 != 0)
                }
            }),
            "video.profile: NVENC did not make the requested {:?} profile in its SPS",
            spec.profile
        );
        SESSION_OPENED.store(true, Ordering::Relaxed);
        Ok(Self {
            native,
            spec: *spec,
            bitstream: vec![0; MAX_PACKET],
            parameters,
            ready: None,
            last: None,
            dropped: 0,
            created_kbps: spec.bitrate_kbps,
            recording: qp.is_some(),
            rate_uncertain: false,
        })
    }

    pub(crate) fn decoder_config(&self) -> Result<Vec<u8>> {
        avcc(&self.parameters)
    }
}

impl VideoEncoder for Encoder {
    type Frame = Frame;

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        ensure!(
            frame.width() == self.spec.width && frame.height() == self.spec.height,
            "video.frame: dimensions must match the encoder's configured size"
        );
        ensure!(
            pts.as_nanos() >= 0 && self.last.is_none_or(|last| pts > last),
            "video.pts: timestamps must be nonnegative and strictly increasing"
        );
        if self.ready.is_some() {
            self.dropped += 1;
            return Ok(());
        }
        let (staging, pitch) = self.native.staging()?;
        let width = self.spec.width as usize;
        bgra_to_nv12(
            &frame.read_bgra()?,
            width * 4,
            staging,
            pitch,
            width,
            self.spec.height as usize,
        )?;
        let mut length = 0;
        let mut output_pts = 0;
        let mut idr = 0;
        let mut error = [0; ERROR_CAPACITY];
        // SAFETY: the staging borrow has ended; the output buffer and all
        // scalar outputs remain alive for this synchronous call.
        let result = unsafe {
            ic_nvenc_encode(
                self.native.0.as_ptr(),
                pts.as_nanos(),
                u32::from(keyframe || self.last.is_none()),
                self.bitstream.as_mut_ptr(),
                self.bitstream.len(),
                &mut length,
                &mut output_pts,
                &mut idr,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        ensure!(result == 0, "{}", error_message(&error));
        ensure!(
            output_pts == pts.as_nanos(),
            "NVENC: reordered or mismatched output timestamp"
        );
        let bytes = self
            .bitstream
            .get(..length)
            .context("NVENC: invalid packet length")?;
        let nals = nals(bytes)?;
        let new_parameters = parameters(&nals);
        if !new_parameters.sps.is_empty() || !new_parameters.pps.is_empty() {
            avcc(&new_parameters)?;
            ensure!(
                !self.recording || new_parameters == self.parameters,
                "NVENC: SPS/PPS changed during a recording; a new file is required"
            );
            self.parameters = new_parameters;
        }
        let mut data = Vec::with_capacity(length);
        let mut picture = false;
        let mut keyframe = false;
        for nal in nals {
            let kind = nal[0] & 31;
            if matches!(kind, 7 | 8) {
                continue;
            }
            picture |= matches!(kind, 1..=5);
            keyframe |= kind == 5;
            data.extend_from_slice(&u32::try_from(nal.len())?.to_be_bytes());
            data.extend_from_slice(nal);
        }
        ensure!(
            picture && keyframe == (idr != 0),
            "NVENC: missing or inconsistent coded picture"
        );
        self.last = Some(pts);
        self.ready = Some(EncodedVideo {
            data,
            pts,
            dts: pts,
            keyframe,
            parameter_sets: keyframe.then(|| self.parameters.clone()),
        });
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        Ok(self.ready.take())
    }

    fn receive_timeout(&mut self, _timeout: Duration) -> Result<Option<EncodedVideo>> {
        self.receive()
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }

    fn rate_uncertain(&self) -> bool {
        self.rate_uncertain
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        ensure!(
            !self.recording,
            "video.rate: cannot change a fixed-quality recording encoder"
        );
        ensure!(
            (1..=60).contains(&fps) && (1..=self.created_kbps).contains(&bitrate_kbps),
            "video.rate: fps must be 1..=60 and bitrate_kbps 1..={}",
            self.created_kbps
        );
        let mut error = [0; ERROR_CAPACITY];
        // SAFETY: exclusive live session and bounded scalar settings/error buffer.
        let result = unsafe {
            ic_nvenc_set_rate(
                self.native.0.as_ptr(),
                bitrate_kbps,
                fps,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        self.rate_uncertain = result != 0;
        ensure!(result == 0, "{}", error_message(&error));
        self.spec.fps = fps;
        self.spec.bitrate_kbps = bitrate_kbps;
        Ok(())
    }
}

pub(crate) fn availability() -> &'static Result<(), String> {
    static AVAILABLE: OnceLock<Result<(), String>> = OnceLock::new();
    static OPENED: Result<(), String> = Ok(());
    let probe = AVAILABLE.get_or_init(|| {
        let result = Encoder::recording(1280, 720, 30, 80)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"));
        if let Err(reason) = &result {
            isocast_diagnostics::output::deferred(
                "info",
                module_path!(),
                format!("NVENC unavailable; using OpenH264: {reason}"),
            );
        }
        result
    });
    // Always finish the initial probe (including its drop) before admitting
    // the first real encoder, even if its native open already set the flag.
    if SESSION_OPENED.load(Ordering::Relaxed) {
        &OPENED
    } else {
        probe
    }
}

/// Limited-range Rec.709, rounding chroma only after summing its 2x2 footprint.
fn bgra_to_nv12(
    src: &[u8],
    stride: usize,
    dst: &mut [u8],
    pitch: usize,
    width: usize,
    height: usize,
) -> Result<()> {
    // Dimensions were bounded and checked even at open. Validate the row
    // layouts once, then use fixed-size pixel pairs without per-pixel checks.
    ensure!(
        stride >= width * 4 && src.len() == stride * height,
        "video.frame: invalid BGRA stride or length"
    );
    ensure!(
        pitch >= width && dst.len() == pitch * (height + height / 2),
        "NVENC: invalid staging pitch or length"
    );
    let (y, uv) = dst.split_at_mut(pitch * height);
    #[cfg(target_arch = "x86_64")]
    let use_avx2 = std::is_x86_feature_detected!("avx2");
    for ((rows, ys), uv) in src
        .chunks_exact(2 * stride)
        .zip(y.chunks_exact_mut(2 * pitch))
        .zip(uv.chunks_exact_mut(pitch))
    {
        let (a, b) = rows.split_at(stride);
        let (ya, yb) = ys.split_at_mut(pitch);
        #[cfg(target_arch = "x86_64")]
        let start = if use_avx2 {
            // SAFETY: AVX2 was detected and the slices cover this row's active pixels.
            unsafe {
                row_avx2(
                    &a[..width * 4],
                    &b[..width * 4],
                    &mut ya[..width],
                    &mut yb[..width],
                    &mut uv[..width],
                )
            };
            width / 8 * 8
        } else {
            0
        };
        #[cfg(not(target_arch = "x86_64"))]
        let start = 0;
        for ((((a, b), ya), yb), uv) in a[start * 4..width * 4]
            .as_chunks::<8>()
            .0
            .iter()
            .zip(b[start * 4..width * 4].as_chunks::<8>().0)
            .zip(ya[start..width].as_chunks_mut::<2>().0)
            .zip(yb[start..width].as_chunks_mut::<2>().0)
            .zip(uv[start..width].as_chunks_mut::<2>().0)
        {
            let luma = |p: &[u8; 8], i: usize| {
                let (b, g, r) = (u16::from(p[i]), u16::from(p[i + 1]), u16::from(p[i + 2]));
                (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)) as u8
            };
            *ya = [luma(a, 0), luma(a, 4)];
            *yb = [luma(b, 0), luma(b, 4)];
            let sum = |i: usize| {
                i32::from(a[i]) + i32::from(a[i + 4]) + i32::from(b[i]) + i32::from(b[i + 4])
            };
            let (b, g, r) = (sum(0), sum(1), sum(2));
            *uv = [
                (128 + ((113 * b - 87 * g - 26 * r + 512) >> 10)) as u8,
                (128 + ((112 * r - 102 * g - 10 * b + 512) >> 10)) as u8,
            ];
        }
    }
    Ok(())
}

/// Eight pixels from each row. All loads/stores are unaligned; the caller
/// handles leftover pairs so there is no padding requirement on either side.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn row_avx2(a: &[u8], b: &[u8], ya: &mut [u8], yb: &mut [u8], uv: &mut [u8]) {
    use std::arch::x86_64::*;
    let byte_pairs = _mm256_broadcastsi128_si256(_mm_setr_epi8(
        0, 4, 1, 5, 2, 6, 3, 7, 8, 12, 9, 13, 10, 14, 11, 15,
    ));
    let y_weights = _mm256_set1_epi32(i32::from_le_bytes([16, 157, 47, 0]));
    let u_weights = _mm256_setr_epi16(
        113, -87, -26, 0, 113, -87, -26, 0, 113, -87, -26, 0, 113, -87, -26, 0,
    );
    let v_weights = _mm256_setr_epi16(
        -10, -102, 112, 0, -10, -102, 112, 0, -10, -102, 112, 0, -10, -102, 112, 0,
    );
    for ((((a, b), ya), yb), uv) in a
        .as_chunks::<32>()
        .0
        .iter()
        .zip(b.as_chunks::<32>().0)
        .zip(ya.as_chunks_mut::<8>().0)
        .zip(yb.as_chunks_mut::<8>().0)
        .zip(uv.as_chunks_mut::<8>().0)
    {
        // SAFETY: each array contains exactly 32 readable bytes; no alignment required.
        let (a, b) = unsafe {
            (
                _mm256_loadu_si256(a.as_ptr().cast()),
                _mm256_loadu_si256(b.as_ptr().cast()),
            )
        };
        // maddubs takes unsigned weights and signed (pixel - 128) values.
        // Each partial sum and the complete centered dot fit i16. Restore
        // the center, limited-range offset and rounding before the unsigned shift.
        let y = |p| _mm256_maddubs_epi16(y_weights, _mm256_xor_si256(p, _mm256_set1_epi8(-128)));
        let luma = _mm256_hadd_epi16(y(a), y(b));
        let luma = _mm256_srli_epi16::<8>(_mm256_add_epi16(
            luma,
            _mm256_set1_epi16(128 * 220 + 16 * 256 + 128),
        ));
        // Each 128-bit lane contains four top followed by four bottom pixels.
        let bytes = _mm_packus_epi16(
            _mm256_castsi256_si128(luma),
            _mm256_extracti128_si256::<1>(luma),
        );
        let rows = _mm_shuffle_epi32::<0xd8>(bytes);
        *ya = _mm_cvtsi128_si64(rows).to_le_bytes();
        *yb = _mm_extract_epi64::<1>(rows).to_le_bytes();

        // BGRA sums for each 2x2 footprint (0..1020). Widen to i32
        // before applying the chroma coefficients; no intermediate average.
        let pairs =
            |p| _mm256_maddubs_epi16(_mm256_shuffle_epi8(p, byte_pairs), _mm256_set1_epi8(1));
        let sums = _mm256_add_epi16(pairs(a), pairs(b));
        let chroma = _mm256_hadd_epi32(
            _mm256_madd_epi16(sums, u_weights),
            _mm256_madd_epi16(sums, v_weights),
        );
        let chroma = _mm256_srli_epi32::<10>(_mm256_add_epi32(
            chroma,
            _mm256_set1_epi32(128 * 1024 + 512),
        ));
        // Two U then two V per lane; pack and interleave both lanes as NV12.
        let words = _mm_packs_epi32(
            _mm256_castsi256_si128(chroma),
            _mm256_extracti128_si256::<1>(chroma),
        );
        let interleaved = _mm_shuffle_epi8(
            words,
            _mm_setr_epi8(0, 4, 2, 6, 8, 12, 10, 14, -1, -1, -1, -1, -1, -1, -1, -1),
        );
        *uv = _mm_cvtsi128_si64(interleaved).to_le_bytes();
    }
}

fn parameters(nals: &[&[u8]]) -> ParameterSets {
    let mut parameters = ParameterSets::default();
    for nal in nals {
        match nal[0] & 31 {
            7 => parameters.sps.push(nal.to_vec()),
            8 => parameters.pps.push(nal.to_vec()),
            _ => {}
        }
    }
    parameters
}

fn nals(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    fn start(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
        (from..bytes.len().saturating_sub(2)).find_map(|at| {
            if bytes[at..].starts_with(&[0, 0, 0, 1]) {
                Some((at, 4))
            } else if bytes[at..].starts_with(&[0, 0, 1]) {
                Some((at, 3))
            } else {
                None
            }
        })
    }
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_PACKET,
        "NVENC: invalid Annex B packet size"
    );
    let (at, prefix) = start(bytes, 0).context("NVENC: missing Annex B start code")?;
    ensure!(
        bytes[..at].iter().all(|byte| *byte == 0),
        "NVENC: invalid Annex B prefix"
    );
    let mut begin = at + prefix;
    let mut result = Vec::new();
    loop {
        let next = start(bytes, begin);
        let mut end = next.map_or(bytes.len(), |(at, _)| at);
        while end > begin && bytes[end - 1] == 0 {
            end -= 1;
        }
        let nal = &bytes[begin..end];
        ensure!(
            !nal.is_empty() && nal[0] & 0x80 == 0 && (1..=23).contains(&(nal[0] & 31)),
            "NVENC: invalid H.264 NAL header"
        );
        result.push(nal);
        ensure!(result.len() <= 1024, "NVENC: too many NALs in one packet");
        let Some((at, prefix)) = next else {
            break;
        };
        begin = at + prefix;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::I420;
    use isocast_engine::FramePool as _;
    use isocast_openh264::openh264;
    use openh264::formats::YUVSource;

    #[test]
    fn annex_b_handles_both_prefixes_and_rejects_invalid_nals() {
        let bytes = [0, 0, 0, 0, 1, 0x67, 100, 0, 31, 0, 0, 1, 0x68, 1, 0, 0];
        let parsed = nals(&bytes).unwrap();
        assert_eq!(parsed, [&[0x67, 100, 0, 31][..], &[0x68, 1][..]]);
        assert!(avcc(&parameters(&parsed)).is_ok());
        for invalid in [
            &[][..],
            &[1, 0, 0, 1, 0x65],
            &[0, 0, 1],
            &[0, 0, 1, 0x80],
            &[0, 0, 1, 0x65, 0, 0, 1],
        ] {
            assert!(nals(invalid).is_err());
        }
    }

    #[test]
    fn nv12_preserves_rec709_planes() {
        let bgra: Vec<_> = [
            [0, 0, 255, 255],
            [0, 0, 255, 255],
            [255, 0, 0, 255],
            [255, 0, 0, 255],
        ]
        .repeat(2)
        .into_iter()
        .flatten()
        .collect();
        let mut nv12 = [0; 12];
        bgra_to_nv12(&bgra, 16, &mut nv12, 4, 4, 2).unwrap();
        assert_eq!(nv12, [63, 63, 32, 32, 63, 63, 32, 32, 102, 240, 241, 118]);
    }

    #[test]
    fn pitched_nv12_matches_i420_bit_for_bit() {
        let mut seed = 12345u32;
        for width in [2, 14, 16, 18, 30, 32, 34, 66, 1920] {
            let height = 18;
            for (stride, pitch) in [(width * 4, width), (width * 4 + 3, width + 7)] {
                for image in 0..5 {
                    let mut src = vec![0x83; stride * height];
                    let mut packed = Vec::new();
                    for (y, row) in src.chunks_exact_mut(stride).enumerate() {
                        for (x, pixel) in row[..width * 4]
                            .as_chunks_mut::<4>()
                            .0
                            .iter_mut()
                            .enumerate()
                        {
                            for (c, byte) in pixel.iter_mut().enumerate() {
                                seed ^= seed << 13;
                                seed ^= seed >> 17;
                                seed ^= seed << 5;
                                *byte = match image {
                                    0 => 0,
                                    1 => 255,
                                    2 => ((x + y * 17 + c * 31) & 255) as u8,
                                    3 => {
                                        if (x ^ y ^ c) & 1 == 0 {
                                            0
                                        } else {
                                            255
                                        }
                                    }
                                    _ => seed as u8,
                                };
                            }
                        }
                        packed.extend_from_slice(&row[..width * 4]);
                    }
                    let mut reference = I420::new(width, height);
                    reference.read_bgra(&packed).unwrap();
                    let mut expected = Vec::new();
                    reference.write_nv12(&mut expected);
                    let mut dst = vec![0xad; pitch * height * 3 / 2];
                    bgra_to_nv12(&src, stride, &mut dst, pitch, width, height).unwrap();
                    for (row, want) in dst.chunks_exact(pitch).zip(expected.chunks_exact(width)) {
                        assert_eq!(
                            &row[..width],
                            want,
                            "width={width} pitch={pitch} image={image}"
                        );
                        assert!(row[width..].iter().all(|&b| b == 0xad));
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "microbenchmark; run with --release --ignored --nocapture"]
    fn benchmark_bgra_to_pitched_nv12_1080p() {
        use std::{hint::black_box, time::Instant};
        let (width, height, pitch) = (1920, 1080, 2048);
        let src: Vec<_> = (0..width * height * 4)
            .map(|i| (i ^ (i >> 8)) as u8)
            .collect();
        let mut i420 = I420::new(width, height);
        let mut packed = Vec::new();
        let mut dst = vec![0; pitch * height * 3 / 2];
        for direct in [false, true] {
            let mut run = || {
                if direct {
                    bgra_to_nv12(black_box(&src), width * 4, &mut dst, pitch, width, height)
                        .unwrap();
                } else {
                    i420.read_bgra(black_box(&src)).unwrap();
                    i420.write_nv12(&mut packed);
                    for (row, input) in dst.chunks_exact_mut(pitch).zip(packed.chunks_exact(width))
                    {
                        row[..width].copy_from_slice(input);
                    }
                }
                black_box(&dst);
            };
            run();
            let start = Instant::now();
            const FRAMES: u32 = 200;
            for _ in 0..FRAMES {
                run();
            }
            eprintln!(
                "{}: {:.3} ms/frame",
                if direct {
                    "direct"
                } else {
                    "I420 + interleave + pitch copy"
                },
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES)
            );
        }
    }

    #[test]
    #[ignore = "requires NVIDIA GPU and NVENC/CUDA driver libraries"]
    fn hardware_threads_admission_timestamps_rate_and_idr() {
        let spec = LiveVideoSpec {
            profile: H264Profile::High,
            width: 320,
            height: 180,
            fps: 30,
            bitrate_kbps: 1000,
            constant_bitrate: true,
            keyframe_interval: 60,
            low_latency: true,
        };
        // Open three simultaneous sessions on this thread, then move them to
        // separate writer threads. CUDA context ownership must follow each call.
        let encoders: Vec<_> = [
            H264Profile::High,
            H264Profile::ConstrainedBaseline,
            H264Profile::ConstrainedBaseline,
        ]
        .into_iter()
        .map(|profile| Encoder::new(&LiveVideoSpec { profile, ..spec }).unwrap())
        .collect();
        let workers: Vec<_> = encoders
            .into_iter()
            .map(|mut encoder| {
                std::thread::spawn(move || {
                    let mut pool = crate::frames::FramePool::new(320, 180).unwrap();
                    let frame = pool
                        .draw(&mut |pixels, _| pixels.fill(255))
                        .unwrap()
                        .unwrap();
                    let first = Time::from_nanos(1_000_000_000);
                    encoder.encode(&frame, first, false).unwrap();
                    encoder
                        .encode(&frame, first.add_ticks(1, 30), false)
                        .unwrap();
                    assert_eq!(encoder.dropped(), 1);
                    let packet = encoder.receive().unwrap().unwrap();
                    assert!(packet.keyframe);
                    assert_eq!((packet.pts, packet.dts), (first, first));
                    let mut annex_b = Vec::new();
                    let parameters = packet.parameter_sets.unwrap();
                    match encoder.spec.profile {
                        H264Profile::High => assert_eq!(parameters.sps[0][1], 100),
                        H264Profile::ConstrainedBaseline => {
                            assert_eq!(parameters.sps[0][1], 66);
                            assert_eq!(parameters.sps[0][2] & 0x40, 0x40);
                            // PPS ids 0, 0 ("1", "1"), then CAVLC ("0").
                            assert_eq!(parameters.pps[0][1] & 0xe0, 0xc0);
                        }
                    }
                    for nal in parameters.sps.iter().chain(&parameters.pps) {
                        annex_b.extend_from_slice(&[0, 0, 0, 1]);
                        annex_b.extend_from_slice(nal);
                    }
                    let mut bytes = packet.data.as_slice();
                    while !bytes.is_empty() {
                        let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                        annex_b.extend_from_slice(&[0, 0, 0, 1]);
                        annex_b.extend_from_slice(&bytes[4..4 + size]);
                        bytes = &bytes[4 + size..];
                    }
                    let mut decoder = openh264::decoder::Decoder::with_api_config(
                        isocast_openh264::api().unwrap(),
                        openh264::decoder::DecoderConfig::new(),
                    )
                    .unwrap();
                    let decoded = decoder.decode(&annex_b).unwrap().unwrap();
                    assert_eq!(decoded.dimensions(), (320, 180));
                    // The independent decoder pads its 320-pixel rows to384;
                    // padding bytes are not part of the decoded picture.
                    assert!(
                        decoded
                            .y()
                            .chunks_exact(decoded.strides().0)
                            .take(180)
                            .all(|row| row[..320].iter().all(|value| (230..=240).contains(value)))
                    );
                    assert!(encoder.receive().unwrap().is_none());
                    assert!(encoder.encode(&frame, first, false).is_err());
                    encoder.set_rate(15, 500).unwrap();
                    assert!(!encoder.rate_uncertain());
                    assert!(encoder.set_rate(0, 500).is_err());
                    assert!(encoder.set_rate(15, 1001).is_err());
                    encoder
                        .encode(&frame, first.add_ticks(2, 30), false)
                        .unwrap();
                    assert!(!encoder.receive().unwrap().unwrap().keyframe);
                    encoder
                        .encode(&frame, first.add_ticks(3, 30), true)
                        .unwrap();
                    assert!(encoder.receive().unwrap().unwrap().keyframe);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        // Run this test alone to check the last-session boundary. NOLOAD
        // observes residency without loading a missing library; balance only
        // this test's extra reference, never the bridge's process-lifetime one.
        // SAFETY: static NUL-terminated name and valid POSIX loader flags.
        let library = unsafe {
            libc::dlopen(
                c"libnvidia-encode.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_NOLOAD,
            )
        };
        assert!(!library.is_null(), "last encoder unloaded the NVENC driver");
        // SAFETY: dlopen above returned this live extra library reference.
        assert_eq!(unsafe { libc::dlclose(library) }, 0);
        // Opening again uses the same retained function table after every
        // previous session has closed, not a new driver initialization.
        drop(Encoder::new(&spec).unwrap());
    }

    #[test]
    #[ignore = "requires NVIDIA GPU and NVENC/CUDA driver libraries"]
    fn hardware_constant_bitrate_padding_and_rate_change() {
        let mut pool = crate::frames::FramePool::new(320, 240).unwrap();
        let frame = pool
            .draw(&mut |pixels, _| pixels.fill(128))
            .unwrap()
            .unwrap();
        let mut encoder = Encoder::new(&LiveVideoSpec {
            profile: H264Profile::High,
            width: 320,
            height: 240,
            fps: 30,
            bitrate_kbps: 1000,
            constant_bitrate: true,
            keyframe_interval: 60,
            low_latency: true,
        })
        .unwrap();
        let mut bytes = [0usize; 6];
        for n in 0..180u32 {
            if n == 90 {
                encoder.set_rate(30, 500).unwrap();
            }
            let pts = Time::from_nanos(1_000_000_000).add_ticks(u64::from(n), 30);
            encoder.encode(&frame, pts, n % 60 == 0).unwrap();
            bytes[(n / 30) as usize] += encoder.receive().unwrap().unwrap().data.len();
        }
        let kbps = |range: std::ops::Range<usize>| {
            bytes[range.clone()].iter().sum::<usize>() as f64 * 8.0 / range.len() as f64 / 1000.0
        };
        assert!((800.0..=1100.0).contains(&kbps(1..3)), "{:?}", bytes);
        assert!((400.0..=560.0).contains(&kbps(4..6)), "{:?}", bytes);
    }
}
