//! Media Foundation software MJPEG decoder for native Motion-JPEG cameras.
//!
//! Layout and lifetime contract:
//! - `JpegDecoder::new` activates one *synchronous software* MJPEG decoder MFT
//!   (enumerated, never a guessed CLSID), accepts exactly the requested
//!   progressive frame size/rate and negotiates a CPU output type: YUY2 when
//!   offered (no extra 4:2:0 downsample), otherwise NV12. The chosen output
//!   layout is fixed for the decoder's lifetime and reported by `layout()`.
//!   Colour defaults to BT.601 full range, the JFIF convention, when the type
//!   does not say otherwise; explicit BT.601/BT.709 and full/limited range are
//!   honoured, anything else is rejected rather than guessed.
//! - `decode` takes one raw compressed frame (one JPEG per sample) and returns
//!   one CPU sample in `layout()`: a single contiguous buffer holding at least
//!   `stride * height` (YUY2) or `stride * height * 3 / 2` (NV12) bytes. The
//!   returned sample owns its buffer independently of later decodes. Timestamps on
//!   the output are the MFT's and must not be used to rebase capture time.
//! - The input sample is only read (its buffer is locked briefly to validate
//!   the JPEG header); the caller keeps it alive until `decode` returns.
//! - Not `Send`: `new`, `decode` and drop must run on one thread that has
//!   joined the MTA and started Media Foundation (`Runtime`).

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface, PWSTR};

use super::pixels::{CameraPixelFormat, Color, Layout};
use crate::mf::Runtime;

/// Compressed frame bounds. A real MJPEG frame is never below a bare SOI/EOI.
const MIN_COMPRESSED: usize = 2;
const MAX_COMPRESSED: usize = 64 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16384;
const MAX_OUTPUT: u64 = 256 * 1024 * 1024;
/// Available output types are tiny lists; never loop unbounded on a bad MFT.
const MAX_TYPES: u32 = 64;

pub(super) struct JpegDecoder {
    transform: IMFTransform,
    /// The exact type accepted at open; re-applied once after a stream change.
    output_type: IMFMediaType,
    layout: Layout,
    width: u32,
    height: u32,
    frame_bytes: u32,
    output_info: MFT_OUTPUT_STREAM_INFO,
    _activation: Activation,
}

impl JpegDecoder {
    pub(super) fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        if width == 0 || width > MAX_DIMENSION || !width.is_multiple_of(2) {
            bail!("camera.decode.width: must be even and in 2..={MAX_DIMENSION}, got {width}");
        }
        if height == 0 || height > MAX_DIMENSION || !height.is_multiple_of(2) {
            bail!("camera.decode.height: must be even and in 2..={MAX_DIMENSION}, got {height}");
        }
        if !(1..=240).contains(&fps) {
            bail!("camera.decode.fps: valid range is 1..=240, got {fps}");
        }
        let _runtime = Runtime::new()?;
        let mut failures = Vec::new();
        for activate in candidates()? {
            let label = name(&activate).unwrap_or_else(|_| "unnamed decoder".into());
            match Self::open_one(Activation(activate), width, height, fps) {
                Ok(decoder) => return Ok(decoder),
                Err(error) => failures.push(format!("{label}: {error:#}")),
            }
        }
        bail!(
            "camera.decode: no usable synchronous software MJPEG decoder{}",
            if failures.is_empty() {
                " is installed".into()
            } else {
                format!(": {}", failures.join("; "))
            }
        )
    }

    /// Layout of every sample `decode` returns. Fixed for this decoder.
    pub(super) fn layout(&self) -> Layout {
        self.layout
    }

    fn open_one(activation: Activation, width: u32, height: u32, fps: u32) -> Result<Self> {
        unsafe {
            let transform: IMFTransform = activation.0.ActivateObject()?;
            let asynchronous = match transform.GetAttributes() {
                Ok(attrs) => optional_u32(&attrs, &MF_TRANSFORM_ASYNC)?.unwrap_or(0) != 0,
                Err(error) if error.code() == E_NOTIMPL => false,
                Err(error) => return Err(error).context("camera.decode: decoder attributes"),
            };
            if asynchronous {
                bail!("camera.decode: asynchronous decoders are not supported");
            }
            let (mut inputs, mut outputs) = (0, 0);
            transform.GetStreamCount(&mut inputs, &mut outputs)?;
            if inputs != 1 || outputs != 1 {
                bail!("camera.decode: decoder must expose one input and one output stream");
            }
            let (mut input_id, mut output_id) = ([0], [0]);
            if let Err(error) = transform.GetStreamIDs(&mut input_id, &mut output_id)
                && error.code() != E_NOTIMPL
            {
                return Err(error.into());
            }
            if input_id[0] != 0 || output_id[0] != 0 {
                bail!("camera.decode: decoder has unsupported nonzero stream ids");
            }

            // Whole progressive frames in, whole frames out: no video or
            // deinterlace processing, no D3D manager (output is CPU memory).
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_MJPG)?;
            input.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )?;
            input.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            transform
                .SetInputType(0, &input, 0)
                .context("camera.decode.format: decoder rejected the MJPEG input type")?;

            let (output_type, layout) = negotiate_output(&transform, width, height)?;
            let frame_bytes = frame_bytes(&layout)?;
            let output_info = transform.GetOutputStreamInfo(0)?;
            if u64::from(output_info.cbSize) > MAX_OUTPUT {
                bail!(
                    "camera.decode.output: decoder requires a {} byte buffer, over the {MAX_OUTPUT} byte limit",
                    output_info.cbSize
                );
            }
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self {
                transform,
                output_type,
                layout,
                width,
                height,
                frame_bytes,
                output_info,
                _activation: activation,
            })
        }
    }

    pub(super) fn decode(&mut self, input: &IMFSample) -> Result<IMFSample> {
        validate_input(input, self.width, self.height)?;
        let result = self.decode_checked(input);
        if result.is_err() {
            // Never leave a half-consumed frame in the MFT for the next one.
            self.reset();
        }
        result
    }

    fn decode_checked(&mut self, input: &IMFSample) -> Result<IMFSample> {
        let mut retried = false;
        loop {
            match unsafe { self.transform.ProcessInput(0, input, 0) } {
                Ok(()) => break,
                Err(error) if error.code() == MF_E_NOTACCEPTING && !retried => {
                    // A stale, unread output blocks input: discard it once.
                    retried = true;
                    self.reset();
                }
                Err(error) => return Err(error).context("camera.decode.input: MFT ProcessInput"),
            }
        }
        let sample = self.output_sample()?;
        let produced = self.process_output(sample)?;
        self.check_output(&produced)?;
        Ok(produced)
    }

    /// Output sample to hand the MFT: None when it provides its own.
    fn output_sample(&mut self) -> Result<Option<IMFSample>> {
        if self.output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0
        {
            return Ok(None);
        }
        let size = self.output_info.cbSize.max(self.frame_bytes);
        let alignment = self.output_info.cbAlignment;
        if alignment > 1 && !alignment.is_power_of_two() {
            bail!("camera.decode.output: unsupported buffer alignment {alignment}");
        }
        let sample = unsafe {
            let sample = MFCreateSample()?;
            let buffer = MFCreateAlignedMemoryBuffer(size, alignment.saturating_sub(1))?;
            sample.AddBuffer(&buffer)?;
            sample
        };
        Ok(Some(sample))
    }

    fn process_output(&mut self, mut sample: Option<IMFSample>) -> Result<IMFSample> {
        let mut reapplied = false;
        loop {
            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample.clone()),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
            };
            // Release both COM fields on every path.
            let produced = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
            drop(unsafe { ManuallyDrop::take(&mut buffer.pEvents) });
            let flags = buffer.dwStatus;
            match result {
                Ok(()) => {}
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE && !reapplied => {
                    reapplied = true;
                    self.reapply_output_type()?;
                    // Provider flags, size and alignment may change even
                    // when the decoded pixel layout stays the same.
                    sample = self.output_sample()?;
                    continue;
                }
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    bail!("camera.decode.format: decoder kept changing its output type");
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    bail!(
                        "camera.decode: decoder produced no frame from a complete JPEG (needs more input)"
                    );
                }
                Err(error) => return Err(error).context("camera.decode.output: MFT ProcessOutput"),
            }
            if flags & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32 != 0 {
                bail!("camera.decode.output: decoder returned an incomplete frame");
            }
            if flags & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32 != 0 {
                bail!("camera.decode.format: decoder changed its output type mid-stream");
            }
            return produced.context("camera.decode.output: decoder returned no sample");
        }
    }

    /// Re-apply the exact negotiated type once; the layout must not move.
    fn reapply_output_type(&mut self) -> Result<()> {
        unsafe {
            self.transform
                .SetOutputType(0, &self.output_type, 0)
                .context("camera.decode.format: decoder rejected its original output type")?;
            let current = self.transform.GetOutputCurrentType(0)?;
            let layout = read_layout(&current, self.width, self.height)?;
            if layout != self.layout {
                bail!(
                    "camera.decode.format: output layout changed from {} to {}",
                    describe(&self.layout),
                    describe(&layout)
                );
            }
            self.output_info = self.transform.GetOutputStreamInfo(0)?;
            if u64::from(self.output_info.cbSize) > MAX_OUTPUT {
                bail!("camera.decode.output: decoder output buffer exceeds {MAX_OUTPUT} bytes");
            }
        }
        Ok(())
    }

    fn check_output(&self, sample: &IMFSample) -> Result<()> {
        unsafe {
            if sample.GetBufferCount()? != 1 {
                bail!("camera.decode.output: decoder returned a multi-buffer sample");
            }
            let buffer = sample.GetBufferByIndex(0)?;
            // No D3D manager was supplied, so GPU surfaces are unexpected and
            // could not be proven safe against output-buffer reuse.
            if buffer.cast::<IMFDXGIBuffer>().is_ok() {
                bail!("camera.decode.output: unexpected GPU surface from a software decoder");
            }
            let length = buffer.GetCurrentLength()?;
            if length < self.frame_bytes {
                bail!(
                    "camera.decode.output: decoder returned {length} bytes, layout {} needs {}",
                    describe(&self.layout),
                    self.frame_bytes
                );
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
    }
}

impl Drop for JpegDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        // `_activation` shuts the object down after the fields above release.
    }
}

struct Activation(IMFActivate);
impl Drop for Activation {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ShutdownObject() };
    }
}

/// Software, synchronous MJPEG decoders, best first. Every returned activation
/// is released when dropped; the CoTaskMem array is freed here.
fn candidates() -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_MJPG,
    };
    let mut list = std::ptr::null_mut();
    let mut count = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
    }
    .context("camera.decode: enumerate MJPEG decoders")?;
    let mut result = Vec::new();
    if !list.is_null() {
        // Move each owned reference out before freeing the array.
        for item in unsafe { std::slice::from_raw_parts_mut(list, count as usize) } {
            if let Some(item) = item.take() {
                result.push(item);
            }
        }
        unsafe { CoTaskMemFree(Some(list.cast())) };
    }
    Ok(result)
}

fn name(activation: &IMFActivate) -> Result<String> {
    let mut value = PWSTR::null();
    let mut len = 0;
    unsafe { activation.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut len) }?;
    let result = if value.is_null() {
        String::new()
    } else {
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value.0, len as usize) })
    };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    Ok(result)
}

/// Pick YUY2 over NV12 from what the decoder offers, apply it and read back
/// the layout the MFT actually committed to.
fn negotiate_output(
    transform: &IMFTransform,
    width: u32,
    height: u32,
) -> Result<(IMFMediaType, Layout)> {
    let (mut yuy2, mut nv12) = (None, None);
    let mut seen = Vec::new();
    for index in 0..MAX_TYPES {
        let ty = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(ty) => ty,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => {
                return Err(error).context("camera.decode.format: list decoder output types");
            }
        };
        let subtype = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }?;
        seen.push(subtype_name(&subtype));
        if subtype == MFVideoFormat_YUY2 && yuy2.is_none() {
            yuy2 = Some(ty);
        } else if subtype == MFVideoFormat_NV12 && nv12.is_none() {
            nv12 = Some(ty);
        }
    }
    let mut failures = Vec::new();
    for ty in [yuy2, nv12].into_iter().flatten() {
        let label = subtype_name(&unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }?);
        let attempt = (|| -> Result<Layout> {
            unsafe {
                transform.SetOutputType(0, &ty, 0)?;
                read_layout(&transform.GetOutputCurrentType(0)?, width, height)
            }
        })();
        match attempt {
            Ok(layout) => return Ok((ty, layout)),
            Err(error) => failures.push(format!("{label}: {error:#}")),
        }
    }
    bail!(
        "camera.decode.format: decoder offers no usable YUY2 or NV12 output ({}); offered: [{}]",
        if failures.is_empty() {
            "neither type present".into()
        } else {
            failures.join("; ")
        },
        seen.join(", ")
    )
}

fn describe(layout: &Layout) -> String {
    format!(
        "{} {}x{} stride {} BT.{} {} range",
        if layout.format == CameraPixelFormat::Nv12 {
            "NV12"
        } else {
            "YUY2"
        },
        layout.width,
        layout.height,
        layout.stride,
        layout.color.matrix,
        if layout.color.full_range {
            "full"
        } else {
            "limited"
        }
    )
}

fn subtype_name(guid: &GUID) -> String {
    if *guid == MFVideoFormat_YUY2 {
        "YUY2".into()
    } else if *guid == MFVideoFormat_NV12 {
        "NV12".into()
    } else {
        format!("{guid:?}")
    }
}

/// Validate a committed output type against the request and derive its layout.
fn read_layout(ty: &IMFMediaType, width: u32, height: u32) -> Result<Layout> {
    unsafe {
        let subtype = ty.GetGUID(&MF_MT_SUBTYPE)?;
        let format = if subtype == MFVideoFormat_YUY2 {
            CameraPixelFormat::Yuy2
        } else if subtype == MFVideoFormat_NV12 {
            CameraPixelFormat::Nv12
        } else {
            bail!(
                "camera.decode.format: output {} is not YUY2 or NV12",
                subtype_name(&subtype)
            );
        };
        let size = ty
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .context("camera.decode.format: output type has no frame size")?;
        if size != ((u64::from(width) << 32) | u64::from(height)) {
            bail!(
                "camera.decode.format: output is {}x{}, requested {width}x{height}",
                size >> 32,
                size & 0xFFFF_FFFF
            );
        }
        if let Some(mode) = optional_u32(ty, &MF_MT_INTERLACE_MODE)?
            && mode != MFVideoInterlace_Progressive.0 as u32
            && mode != MFVideoInterlace_Unknown.0 as u32
        {
            bail!("camera.decode.format: output interlace mode {mode} is not progressive");
        }
        let row = match format {
            CameraPixelFormat::Yuy2 => width.checked_mul(2),
            _ => Some(width),
        }
        .context("camera.decode.format: row size overflow")?;
        let stride = match optional_u32(ty, &MF_MT_DEFAULT_STRIDE)? {
            // The attribute stores an int32; negative means bottom-up.
            Some(value) => value as i32,
            None => MFGetStrideForBitmapInfoHeader(subtype.data1, width)
                .context("camera.decode.format: output stride")?,
        };
        if stride <= 0 || (stride as u32) < row {
            bail!(
                "camera.decode.format: stride {stride} must be top-down and at least {row} bytes"
            );
        }
        let matrix = match optional_u32(ty, &MF_MT_YUV_MATRIX)? {
            None => 601,
            Some(n) if n == MFVideoTransferMatrix_BT601.0 as u32 || n == 0 => 601,
            Some(n) if n == MFVideoTransferMatrix_BT709.0 as u32 => 709,
            Some(n) => bail!(
                "camera.decode.format.matrix: unsupported YUV matrix {n}; valid: BT.601 ({}), BT.709 ({})",
                MFVideoTransferMatrix_BT601.0,
                MFVideoTransferMatrix_BT709.0
            ),
        };
        let full_range = match optional_u32(ty, &MF_MT_VIDEO_NOMINAL_RANGE)? {
            None => true,
            Some(n) if n == MFNominalRange_Unknown.0 as u32 => true,
            Some(n) if n == MFNominalRange_0_255.0 as u32 => true,
            Some(n) if n == MFNominalRange_16_235.0 as u32 => false,
            Some(n) => bail!(
                "camera.decode.format.range: unsupported nominal range {n}; valid: full ({}), limited ({})",
                MFNominalRange_0_255.0,
                MFNominalRange_16_235.0
            ),
        };
        let layout = Layout {
            format,
            width,
            height,
            stride,
            color: Color { matrix, full_range },
        };
        frame_bytes(&layout)?;
        Ok(layout)
    }
}

/// Bytes a complete frame needs: packed YUY2, or NV12's luma plus half-height
/// interleaved chroma at the same stride. Bounded by `MAX_OUTPUT`.
fn frame_bytes(layout: &Layout) -> Result<u32> {
    let rows = match layout.format {
        CameraPixelFormat::Yuy2 => u64::from(layout.height) * 2,
        CameraPixelFormat::Nv12 => u64::from(layout.height) * 3,
        _ => bail!("camera.decode.format: output must be YUY2 or NV12"),
    };
    let bytes = u64::from(layout.stride as u32)
        .checked_mul(rows)
        .map(|n| n / 2)
        .filter(|n| *n <= MAX_OUTPUT)
        .with_context(|| {
            format!("camera.decode.format: frame exceeds the {MAX_OUTPUT} byte output limit")
        })?;
    Ok(bytes as u32)
}

fn optional_u32(attrs: &IMFAttributes, key: &GUID) -> Result<Option<u32>> {
    match unsafe { attrs.GetUINT32(key) } {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Locked media buffer: unlocks on every path.
struct Locked<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *mut u8,
    len: usize,
}
impl<'a> Locked<'a> {
    fn new(buffer: &'a IMFMediaBuffer) -> Result<Self> {
        let (mut data, mut max, mut len) = (std::ptr::null_mut(), 0, 0);
        unsafe { buffer.Lock(&mut data, Some(&mut max), Some(&mut len)) }
            .context("camera.decode.input: lock compressed buffer")?;
        let locked = Self {
            buffer,
            data,
            len: len as usize,
        };
        if data.is_null() || len > max {
            // `locked` drops here and unlocks.
            return Err(anyhow!("camera.decode.input: invalid compressed buffer"));
        }
        Ok(locked)
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: Lock returned `data` valid for `len` initialised bytes until
        // Unlock, which only happens in Drop after this borrow ends.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.buffer.Unlock() };
    }
}

fn validate_input(sample: &IMFSample, width: u32, height: u32) -> Result<()> {
    let buffer = unsafe {
        // Do not rewrite the caller's sample when it is already one buffer.
        if sample.GetBufferCount()? == 1 {
            sample.GetBufferByIndex(0)?
        } else {
            sample.ConvertToContiguousBuffer()?
        }
    };
    let length = unsafe { buffer.GetCurrentLength() }? as usize;
    if !(MIN_COMPRESSED..=MAX_COMPRESSED).contains(&length) {
        bail!(
            "camera.decode.input: compressed frame is {length} bytes; valid range is {MIN_COMPRESSED}..={MAX_COMPRESSED}"
        );
    }
    let locked = Locked::new(&buffer)?;
    validate_jpeg(locked.bytes(), width, height)
}

/// Check the JPEG's SOF header against the requested size before the MFT sees
/// it, so a corrupt frame cannot make the decoder size buffers from the data.
/// Markers are walked with checked arithmetic and never past the first SOS.
fn validate_jpeg(bytes: &[u8], width: u32, height: u32) -> Result<()> {
    if bytes.len() < MIN_COMPRESSED || bytes[..2] != [0xFF, 0xD8] {
        bail!("camera.decode.input: not a JPEG (missing SOI marker)");
    }
    let mut pos = 2usize;
    let mut seen_sof = false;
    loop {
        // Markers may be preceded by any number of 0xFF fill bytes.
        if bytes.get(pos) != Some(&0xFF) {
            bail!("camera.decode.input: expected a JPEG marker at byte {pos}");
        }
        while bytes.get(pos) == Some(&0xFF) {
            pos += 1;
        }
        let marker = *bytes
            .get(pos)
            .context("camera.decode.input: JPEG ends inside a marker")?;
        pos += 1;
        match marker {
            0x00 => bail!("camera.decode.input: stuffed byte where a JPEG marker was expected"),
            // Standalone markers (TEM, RSTn) carry no length.
            0x01 | 0xD0..=0xD7 => continue,
            0xD8 => bail!("camera.decode.input: unexpected second SOI"),
            0xD9 => bail!("camera.decode.input: JPEG ends before any scan"),
            _ => {}
        }
        let end = pos
            .checked_add(2)
            .filter(|end| *end <= bytes.len())
            .context("camera.decode.input: JPEG ends inside a segment length")?;
        let length = usize::from(u16::from_be_bytes([bytes[pos], bytes[pos + 1]]));
        let segment_end = pos
            .checked_add(length)
            .filter(|end| length >= 2 && *end <= bytes.len())
            .with_context(|| {
                format!(
                    "camera.decode.input: JPEG segment {marker:#04x} has invalid length {length}"
                )
            })?;
        let body = &bytes[end..segment_end];
        match marker {
            // Baseline, extended sequential and progressive Huffman.
            0xC0..=0xC2 => {
                if seen_sof {
                    bail!("camera.decode.input: JPEG has more than one frame header");
                }
                seen_sof = true;
                if body.len() < 6 {
                    bail!("camera.decode.input: JPEG frame header is truncated");
                }
                let components = usize::from(body[5]);
                if !matches!(components, 1 | 3) || body.len() != 6 + 3 * components {
                    bail!(
                        "camera.decode.input: JPEG has {components} components (valid: 1 or 3) or a malformed frame header"
                    );
                }
                if body[0] != 8 {
                    bail!(
                        "camera.decode.input: JPEG precision is {}; only 8-bit is supported",
                        body[0]
                    );
                }
                let jpeg_height = u32::from(u16::from_be_bytes([body[1], body[2]]));
                let jpeg_width = u32::from(u16::from_be_bytes([body[3], body[4]]));
                if (jpeg_width, jpeg_height) != (width, height) {
                    bail!(
                        "camera.decode.input: JPEG is {jpeg_width}x{jpeg_height}, requested {width}x{height}"
                    );
                }
            }
            // Lossless, differential/hierarchical and arithmetic-coded SOFs.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => bail!(
                "camera.decode.input: unsupported JPEG process (SOF {marker:#04x}); valid: baseline or progressive Huffman"
            ),
            // Entropy-coded data starts after SOS; stop here.
            0xDA => {
                if !seen_sof {
                    bail!("camera.decode.input: JPEG scan precedes its frame header");
                }
                return Ok(());
            }
            _ => {}
        }
        pos = segment_end;
    }
}
