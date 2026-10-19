//! Camera pictures of any raw Media Foundation format, delivered as GPU frames.
//!
//! * NV12 that is Rec.709 limited range stays NV12 (`Delivery::Native*`): a GPU
//!   subresource copy for a DXGI sample, or one `UpdateSubresource` of both
//!   planes for a CPU sample. Nothing is converted.
//! * Every other raw format (NV12 with another matrix or full range, YUY2,
//!   UYVY, RGB24, RGB32, ARGB32) is converted to BGRA by one pixel-shader draw
//!   into an `OutputPool` frame (`Delivery::Converted*`). CPU samples are sent
//!   to the GPU as raw bytes with one `UpdateSubresource` (a raw
//!   `ByteAddressBuffer`, so RGB24 works at any size) and are never converted
//!   or expanded on the CPU. DXGI samples are never locked or read back: the
//!   visible region is copied into a private sampleable texture on the same
//!   immediate context, then drawn. A format with no GPU carrier (RGB24, UYVY)
//!   is an error, not a download.
//! * Camera alpha is ignored; every converted picture is opaque.
//!
//! The caller keeps the source `IMFSample` alive until the returned frame's
//! ready fence completes, and on ANY error quarantines this whole object and
//! the sample: a failed call may already have queued GPU work.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_BUFFEREX,
    D3D11_SRV_DIMENSION_TEXTURE2D, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{GUID, Interface, PCSTR, s};

use crate::frames::{Frame, GraphicsDevice, TexturePool};
use crate::output::OutputPool;

const MAX_DIMENSION: u32 = D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION;
/// Largest CPU picture accepted, in bytes actually read by the GPU.
const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;

const SOURCE_BUFFER: u32 = 0;
const SOURCE_TEXTURE: u32 = 1;

/// A raw (or JPEG) camera pixel layout, as named by its Media Foundation subtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPixelFormat {
    Nv12,
    Yuy2,
    Uyvy,
    Rgb24,
    Rgb32,
    Argb32,
    Mjpg,
}

impl CameraPixelFormat {
    const NAMES: &'static str = "NV12, YUY2, UYVY, RGB24, RGB32, ARGB32, MJPG";

    pub fn name(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::Yuy2 => "YUY2",
            Self::Uyvy => "UYVY",
            Self::Rgb24 => "RGB24",
            Self::Rgb32 => "RGB32",
            Self::Argb32 => "ARGB32",
            Self::Mjpg => "MJPG",
        }
    }

    pub(super) fn from_guid(guid: GUID) -> Result<Self> {
        Ok(if guid == MFVideoFormat_NV12 {
            Self::Nv12
        } else if guid == MFVideoFormat_YUY2 {
            Self::Yuy2
        } else if guid == MFVideoFormat_UYVY {
            Self::Uyvy
        } else if guid == MFVideoFormat_RGB24 {
            Self::Rgb24
        } else if guid == MFVideoFormat_RGB32 {
            Self::Rgb32
        } else if guid == MFVideoFormat_ARGB32 {
            Self::Argb32
        } else if guid == MFVideoFormat_MJPG {
            Self::Mjpg
        } else {
            bail!(
                "camera.format: unsupported pixel format {guid:?}; valid formats: {}",
                Self::NAMES
            )
        })
    }

    pub(super) fn from_name(name: &str) -> Result<Self> {
        [
            Self::Nv12,
            Self::Yuy2,
            Self::Uyvy,
            Self::Rgb24,
            Self::Rgb32,
            Self::Argb32,
            Self::Mjpg,
        ]
        .into_iter()
        .find(|format| format.name().eq_ignore_ascii_case(name))
        .with_context(|| {
            format!(
                "camera.format: unknown pixel format {name:?}; valid formats: {}",
                Self::NAMES
            )
        })
    }

    /// Lower is preferred when a camera offers several formats at one size.
    /// NV12 first: it can reach the GPU untouched. Then raw uncompressed
    /// formats by how little the driver or this code must reinterpret them:
    /// packed 4:2:2 YUV, then RGB (RGB32/ARGB32 before the 3-byte RGB24).
    /// Lossy JPEG is always last: it costs a CPU decode and a second chroma
    /// downsampling.
    pub(super) fn rank(self) -> u8 {
        match self {
            Self::Nv12 => 0,
            Self::Yuy2 => 1,
            Self::Uyvy => 2,
            Self::Rgb32 => 3,
            Self::Argb32 => 4,
            Self::Rgb24 => 5,
            Self::Mjpg => 6,
        }
    }

    /// Bytes of one picture row of `width` pixels, without padding. For NV12
    /// this is the luma row, which is also the width of one chroma row.
    pub(super) fn row_bytes(self, width: u32) -> Result<u32> {
        let bytes = match self {
            Self::Nv12 => Some(width),
            Self::Yuy2 | Self::Uyvy => width.checked_mul(2),
            Self::Rgb24 => width.checked_mul(3),
            Self::Rgb32 | Self::Argb32 => width.checked_mul(4),
            Self::Mjpg => bail!(
                "camera.format: MJPG has no pixel rows; decode it to NV12 or YUY2 first (valid raw formats: NV12, YUY2, UYVY, RGB24, RGB32, ARGB32)"
            ),
        };
        bytes.context("camera.format: row size overflow")
    }

    fn is_rgb(self) -> bool {
        matches!(self, Self::Rgb24 | Self::Rgb32 | Self::Argb32)
    }

    /// The DXGI texture formats that can carry this format on the GPU.
    fn carries(self, dxgi: DXGI_FORMAT) -> bool {
        match self {
            Self::Nv12 => dxgi == DXGI_FORMAT_NV12,
            Self::Yuy2 => dxgi == DXGI_FORMAT_YUY2,
            Self::Rgb32 | Self::Argb32 => {
                dxgi == DXGI_FORMAT_B8G8R8A8_UNORM || dxgi == DXGI_FORMAT_B8G8R8X8_UNORM
            }
            Self::Uyvy | Self::Rgb24 | Self::Mjpg => false,
        }
    }
}

/// How YUV codes become RGB. RGB ignores the matrix, but honors the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Color {
    /// 601 or 709.
    pub matrix: u32,
    pub full_range: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Layout {
    pub format: CameraPixelFormat,
    pub width: u32,
    pub height: u32,
    /// Default row pitch in bytes, used only for CPU buffers that do not report
    /// their own: the Media Foundation default stride, negative for bottom-up
    /// RGB. Zero when unknown, which makes such a buffer an error.
    pub stride: i32,
    pub color: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    /// NV12 copied between GPU textures.
    NativeGpu,
    /// NV12 uploaded from the CPU, both planes in one update.
    NativeUpload,
    /// GPU texture converted to BGRA by a shader.
    ConvertedGpu,
    /// CPU bytes converted to BGRA by a shader.
    ConvertedUpload,
}

pub(super) struct CameraPixels {
    device: GraphicsDevice,
    uncertain: bool,
    layout: Layout,
    textures: TexturePool,
    output: OutputPool,
    draw: Option<Draw>,
    /// Reused only to zero-pad a CPU payload to a whole number of 32-bit words.
    staging: Vec<u8>,
    /// Reused destination of `IMF2DBuffer::ContiguousCopyTo`.
    contiguous: Vec<u8>,
}

impl CameraPixels {
    pub(super) fn new(device: GraphicsDevice, layout: Layout) -> Result<Self> {
        check_layout(&layout)?;
        Ok(Self {
            device: device.clone(),
            uncertain: false,
            layout,
            textures: TexturePool::new(device.clone()),
            output: OutputPool::new(device, layout.width, layout.height)?,
            draw: None,
            staging: Vec::new(),
            contiguous: Vec::new(),
        })
    }

    /// Deliver one Media Foundation sample. `Ok(None)` means every output or
    /// texture slot is still in use and the picture was dropped.
    pub(super) fn sample(&mut self, sample: &IMFSample) -> Result<Option<(Frame, Delivery)>> {
        if self.uncertain {
            bail!("camera.frame: previous GPU write is unconfirmed; reopen the process")
        }
        let count = unsafe { sample.GetBufferCount() }.context("camera.frame: buffer count")?;
        if count == 0 || count > 64 {
            bail!("camera.frame: sample buffer count {count}; valid range1..=64");
        }
        let buffer = if count == 1 {
            unsafe { sample.GetBufferByIndex(0) }.context("camera.frame: sample buffer")?
        } else {
            // Merging would copy GPU buffers through the CPU.
            for index in 0..count {
                let part = unsafe { sample.GetBufferByIndex(index) }
                    .context("camera.frame: sample buffer")?;
                if part.cast::<IMFDXGIBuffer>().is_ok() {
                    bail!(
                        "camera.frame: a sample with {count} buffers including a GPU buffer is not supported; requires exactly one DXGI buffer"
                    );
                }
            }
            let total =
                unsafe { sample.GetTotalLength() }.context("camera.frame: sample length")?;
            if u64::from(total) > MAX_PAYLOAD {
                bail!("camera.frame: sample of {total} bytes exceeds the 256 MiB bound");
            }
            unsafe { sample.ConvertToContiguousBuffer() }
                .context("camera.frame: merge CPU sample buffers")?
        };
        // A DXGI buffer is NEVER locked: that would map the texture to the CPU.
        match buffer.cast::<IMFDXGIBuffer>() {
            Ok(dxgi) => self.gpu(&dxgi),
            Err(_) => self.cpu(&buffer),
        }
    }

    /// Deliver CPU pixels. `bytes` starts at the lowest address of the image
    /// including row padding; `stride` is the signed row pitch. A negative
    /// stride (RGB only) means the rows are stored bottom-up, with the
    /// bottom row first. The bytes are copied to the GPU before this returns.
    pub(super) fn bytes(&mut self, bytes: &[u8], stride: i32) -> Result<Option<(Frame, Delivery)>> {
        if self.uncertain {
            bail!("camera.frame: previous GPU write is unconfirmed; reopen the process")
        }
        let layout = self.layout;
        let pitch = check_pitch(&layout, stride)?;
        let required = required_len(&layout, pitch)?;
        if (bytes.len() as u64) < required {
            bail!(
                "camera.frame: {} {}x{} with stride {stride} needs {required} bytes, got {}",
                layout.format.name(),
                layout.width,
                layout.height,
                bytes.len()
            );
        }
        if layout.format == CameraPixelFormat::Nv12 && is_native_nv12(&layout) {
            // Both planes in one upload. Pad only the unused tail when needed;
            // buffer padding must never change the delivered pixel format.
            let whole = u64::from(pitch) * u64::from(layout.height + layout.height / 2);
            if whole > MAX_PAYLOAD {
                bail!("camera.frame: padded NV12 exceeds256MiB")
            }
            let data = if bytes.len() as u64 >= whole {
                bytes
            } else {
                self.staging.clear();
                self.staging.try_reserve_exact(whole as usize)?;
                self.staging.extend_from_slice(&bytes[..required as usize]);
                self.staging.resize(whole as usize, 0);
                &self.staging
            };
            self.uncertain = true;
            let result = self
                .textures
                .upload_nv12(layout.width, layout.height, data, pitch);
            if result.is_ok() {
                self.uncertain = false;
            }
            return Ok(result?.map(|frame| (frame, Delivery::NativeUpload)));
        }
        // `required` is below 256 MiB. Upload whole 32-bit words: from the
        // caller's memory when it extends far enough, else from a zero-padded
        // copy of exactly the bytes the picture uses.
        let words = required.next_multiple_of(4) as usize;
        let data: &[u8] = if words <= bytes.len() {
            &bytes[..words]
        } else {
            self.staging.clear();
            self.staging
                .try_reserve_exact(words)
                .context("camera.frame: could not allocate staging bytes")?;
            self.staging.extend_from_slice(&bytes[..required as usize]);
            self.staging.resize(words, 0);
            &self.staging
        };
        if self.draw.is_none() {
            self.draw = Some(Draw::new(&self.device, &layout)?);
        }
        self.draw
            .as_mut()
            .expect("made above")
            .ensure_upload(words)?;
        let params = params(&layout, pitch, stride < 0);
        let draw = self.draw.as_mut().expect("made above");
        self.uncertain = true;
        let frame = self
            .output
            .render(|context, target| draw.run_buffer(context, target, data, &params, &layout));
        if frame.is_ok() {
            self.uncertain = false;
        }
        Ok(frame?.map(|frame| (frame, Delivery::ConvertedUpload)))
    }

    pub(super) fn uncertain(&self) -> bool {
        self.uncertain
    }

    fn gpu(&mut self, dxgi: &IMFDXGIBuffer) -> Result<Option<(Frame, Delivery)>> {
        let layout = self.layout;
        let mut raw = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
            .context("camera.frame: DXGI buffer texture")?;
        if raw.is_null() {
            bail!("camera.frame: null D3D11 texture");
        }
        // GetResource transferred this owned COM reference to the caller.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource =
            unsafe { dxgi.GetSubresourceIndex() }.context("camera.frame: DXGI subresource")?;
        let owner: ID3D11Device5 = unsafe { texture.GetDevice() }?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if !layout.format.carries(desc.Format) {
            bail!(
                "camera.frame: {} cannot be carried by DXGI format {:?}; GPU carriers are NV12 for NV12, YUY2 for YUY2 and B8G8R8A8/B8G8R8X8 for RGB32/ARGB32. UYVY and RGB24 have no GPU carrier, and a CPU readback is never used",
                layout.format.name(),
                desc.Format
            );
        }
        if owner != *self.device.raw()
            || desc.Usage != D3D11_USAGE_DEFAULT
            || desc.CPUAccessFlags != 0
            || desc.MipLevels != 1
            || desc.ArraySize == 0
            || subresource >= desc.ArraySize
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
            || desc.Width < layout.width
            || desc.Height < layout.height
            || (layout.format != CameraPixelFormat::Rgb32
                && layout.format != CameraPixelFormat::Argb32
                && (!desc.Width.is_multiple_of(2) || !desc.Height.is_multiple_of(2)))
        {
            bail!(
                "camera.frame: requires a same-device, GPU-only default-usage, single-mip, non-multisampled texture with a valid array subresource at least {}x{} (even for YUV); got {desc:?}, subresource {subresource}",
                layout.width,
                layout.height
            );
        }
        if is_native_nv12(&layout) {
            self.uncertain = true;
            let result =
                self.textures
                    .copy_subresource(&texture, subresource, layout.width, layout.height);
            if result.is_ok() {
                self.uncertain = false;
            }
            return Ok(result?.map(|frame| (frame, Delivery::NativeGpu)));
        }
        if self.draw.is_none() {
            self.draw = Some(Draw::new(&self.device, &layout)?);
        }
        self.draw
            .as_mut()
            .expect("made above")
            .prepare_copy(desc.Format, &layout)?;
        let params = params(&layout, 0, false);
        let draw = self.draw.as_mut().expect("made above");
        self.uncertain = true;
        let frame = self.output.render(|context, target| {
            draw.run_texture(context, target, &texture, subresource, &params, &layout)
        });
        if frame.is_ok() {
            self.uncertain = false;
        }
        Ok(frame?.map(|frame| (frame, Delivery::ConvertedGpu)))
    }

    fn cpu(&mut self, buffer: &IMFMediaBuffer) -> Result<Option<(Frame, Delivery)>> {
        let layout = self.layout;
        if let Ok(buffer) = buffer.cast::<IMF2DBuffer2>() {
            let mut scanline0 = std::ptr::null_mut();
            let mut pitch = 0;
            let mut start = std::ptr::null_mut();
            let mut length = 0;
            unsafe {
                buffer.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scanline0,
                    &mut pitch,
                    &mut start,
                    &mut length,
                )
            }
            .context("camera.frame: lock 2-D buffer")?;
            let _lock = Locked2D(&buffer);
            let (low, len) = region(&layout, scanline0, pitch, start, length)?;
            // SAFETY: `region` proved [low, low + len) lies inside the locked
            // buffer, which stays locked until `_lock` drops after the upload.
            let bytes = unsafe { std::slice::from_raw_parts(low, len) };
            return self.bytes(bytes, pitch);
        }
        if let Ok(buffer) = buffer.cast::<IMF2DBuffer>() {
            // Only the contiguous form is available: it uses the standard stride.
            let stride = default_stride(&layout)?;
            let required = required_len(&layout, check_pitch(&layout, stride)?)?;
            let length = u64::from(
                unsafe { buffer.GetContiguousLength() }
                    .context("camera.frame: 2-D buffer contiguous length")?,
            );
            if length < required || length > MAX_PAYLOAD {
                bail!(
                    "camera.frame: contiguous 2-D buffer has {length} bytes; {} {}x{} with stride {stride} needs {required} and at most 256 MiB is accepted",
                    layout.format.name(),
                    layout.width,
                    layout.height
                );
            }
            let mut copy = std::mem::take(&mut self.contiguous);
            copy.clear();
            copy.try_reserve_exact(length as usize)
                .context("camera.frame: could not allocate contiguous bytes")?;
            copy.resize(length as usize, 0);
            let result = unsafe { buffer.ContiguousCopyTo(&mut copy) }
                .context("camera.frame: contiguous copy of 2-D buffer")
                .and_then(|()| self.bytes(&copy, stride));
            self.contiguous = copy;
            return result;
        }
        // A flat buffer has no layout of its own: use the format's default.
        let stride = default_stride(&layout)?;
        let mut data = std::ptr::null_mut();
        let mut maximum = 0;
        let mut current = 0;
        unsafe { buffer.Lock(&mut data, Some(&mut maximum), Some(&mut current)) }
            .context("camera.frame: lock buffer")?;
        let _lock = LockedFlat(buffer);
        if data.is_null() {
            bail!("camera.frame: null CPU buffer");
        }
        // SAFETY: the locked buffer holds at least min(current, maximum) bytes
        // until `_lock` drops.
        let bytes = unsafe { std::slice::from_raw_parts(data, current.min(maximum) as usize) };
        self.bytes(bytes, stride)
    }
}

struct Locked2D<'a>(&'a IMF2DBuffer2);

impl Drop for Locked2D<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Unlock2D() };
    }
}

struct LockedFlat<'a>(&'a IMFMediaBuffer);

impl Drop for LockedFlat<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.Unlock() };
    }
}

fn check_layout(layout: &Layout) -> Result<()> {
    for (field, size) in [("width", layout.width), ("height", layout.height)] {
        if !(1..=MAX_DIMENSION).contains(&size) {
            bail!("camera.layout.{field}: {size} is outside 1..={MAX_DIMENSION}");
        }
    }
    // Rejects MJPG, which the caller must decode first.
    layout.format.row_bytes(layout.width)?;
    let even = match layout.format {
        CameraPixelFormat::Nv12 => {
            layout.width.is_multiple_of(2) && layout.height.is_multiple_of(2)
        }
        CameraPixelFormat::Yuy2 | CameraPixelFormat::Uyvy => layout.width.is_multiple_of(2),
        _ => true,
    };
    if !even {
        bail!(
            "camera.layout: {} requires an even width{} but the picture is {}x{}",
            layout.format.name(),
            if layout.format == CameraPixelFormat::Nv12 {
                " and height"
            } else {
                ""
            },
            layout.width,
            layout.height
        );
    }
    if !matches!(layout.color.matrix, 601 | 709) {
        bail!(
            "camera.layout.color.matrix: {} is not supported; valid matrices: 601, 709",
            layout.color.matrix
        );
    }
    if layout.stride != 0 {
        required_len(layout, check_pitch(layout, layout.stride)?)?;
    }
    Ok(())
}

/// Validate a signed row pitch and return its magnitude.
fn check_pitch(layout: &Layout, stride: i32) -> Result<u32> {
    let pitch = stride.unsigned_abs();
    let row = layout.format.row_bytes(layout.width)?;
    if pitch < row {
        bail!(
            "camera.frame.stride: {stride} is shorter than one {} row of {row} bytes",
            layout.format.name()
        );
    }
    if stride < 0 && !layout.format.is_rgb() {
        bail!(
            "camera.frame.stride: {stride} is negative; only RGB24, RGB32 and ARGB32 can be bottom-up"
        );
    }
    Ok(pitch)
}

fn default_stride(layout: &Layout) -> Result<i32> {
    if layout.stride == 0 {
        bail!(
            "camera.layout.stride: the {} stride is unknown, and a buffer without its own layout needs it (the Media Foundation default stride, negative for bottom-up RGB)",
            layout.format.name()
        );
    }
    Ok(layout.stride)
}

/// Bytes from the lowest address through the last pixel byte; the final row
/// needs no padding. NV12 has half as many chroma rows after the luma rows.
fn required_len(layout: &Layout, pitch: u32) -> Result<u64> {
    let row = u64::from(layout.format.row_bytes(layout.width)?);
    let mut rows = u64::from(layout.height);
    if layout.format == CameraPixelFormat::Nv12 {
        rows += u64::from(layout.height / 2);
    }
    let length = u64::from(pitch) * (rows - 1) + row;
    if length > MAX_PAYLOAD {
        bail!(
            "camera.frame: {} {}x{} needs {length} bytes; at most 256 MiB is accepted",
            layout.format.name(),
            layout.width,
            layout.height
        );
    }
    Ok(length)
}

/// The lowest-address range a locked 2-D buffer must expose for this picture.
fn region(
    layout: &Layout,
    scanline0: *mut u8,
    pitch: i32,
    start: *mut u8,
    length: u32,
) -> Result<(*const u8, usize)> {
    if scanline0.is_null() || start.is_null() {
        bail!("camera.frame: 2-D buffer returned a null address");
    }
    let magnitude = check_pitch(layout, pitch)?;
    let required = required_len(layout, magnitude)?;
    let first = scanline0 as usize;
    let begin = start as usize;
    let end = begin
        .checked_add(length as usize)
        .context("camera.frame: 2-D buffer range overflow")?;
    // Scanline 0 is the TOP row; a negative pitch stores it at the highest
    // address, so the image begins (bottom row first) lower in memory.
    let low = if pitch > 0 {
        Some(first)
    } else {
        first.checked_sub((layout.height - 1) as usize * magnitude as usize)
    };
    let high = low.and_then(|low| low.checked_add(required as usize));
    match (low, high) {
        (Some(low), Some(high)) if low >= begin && high <= end => {
            Ok((low as *const u8, required as usize))
        }
        _ => bail!(
            "camera.frame: pitch {pitch} places a {}x{} {} picture outside its {length}-byte 2-D buffer",
            layout.width,
            layout.height,
            layout.format.name()
        ),
    }
}

fn is_native_nv12(layout: &Layout) -> bool {
    layout.format == CameraPixelFormat::Nv12
        && layout.color.matrix == 709
        && !layout.color.full_range
}

type Params = [u32; 8];

/// Constant-buffer words, in the order of `cbuffer Params`.
fn params(layout: &Layout, pitch: u32, bottom_up: bool) -> Params {
    [
        layout.width,
        layout.height,
        pitch,
        u32::from(bottom_up),
        pitch.wrapping_mul(layout.height),
        u32::from(layout.color.matrix == 709),
        u32::from(layout.color.full_range),
        0,
    ]
}

// Pixel-shader conversion to opaque BGRA. SOURCE 0 reads a raw byte buffer
// (rows `stride` apart, bottom-up when `flip`, NV12 chroma at `uv_offset`);
// SOURCE 1 reads a texture. FORMAT: 0 NV12, 1 YUY2, 2 UYVY, 3 RGB24, 4 RGB32
// and ARGB32. Chroma is replicated, not interpolated.
const SHADER: &str = r#"
cbuffer Params : register(b0) {
    uint width;
    uint height;
    uint stride;
    uint flip;
    uint uv_offset;
    uint bt709;
    uint full_range;
    uint reserved;
};

float4 vs(uint id : SV_VertexID) : SV_Position {
    return float4(id == 2 ? 3.0 : -1.0, id == 1 ? 3.0 : -1.0, 0.0, 1.0);
}

// Codes are 8-bit values in 0..255.
float3 rgb_from_yuv(float y, float u, float v) {
    float luma = full_range != 0 ? y / 255.0 : (y - 16.0) / 219.0;
    float2 c = (float2(u, v) - 128.0) / (full_range != 0 ? 255.0 : 224.0);
    float kr = bt709 != 0 ? 0.2126 : 0.299;
    float kb = bt709 != 0 ? 0.0722 : 0.114;
    float r = luma + 2.0 * (1.0 - kr) * c.y;
    float b = luma + 2.0 * (1.0 - kb) * c.x;
    float g = (luma - kr * r - kb * b) / (1.0 - kr - kb);
    return saturate(float3(r, g, b));
}

float3 rgb_from_codes(float3 c) {
    return saturate(full_range != 0 ? c / 255.0 : (c - 16.0) / 219.0);
}

#if SOURCE == 0

ByteAddressBuffer source : register(t0);

uint byte_at(uint offset) {
    return (source.Load(offset & ~3u) >> ((offset & 3u) * 8u)) & 0xffu;
}

// Four bytes from any offset, first byte in the low bits. Loads past the end
// of the view return zero.
uint word_at(uint offset) {
    uint shift = (offset & 3u) * 8u;
    uint low = source.Load(offset & ~3u);
    if (shift == 0u) {
        return low;
    }
    return (low >> shift) | (source.Load((offset & ~3u) + 4u) << (32u - shift));
}

float4 ps(float4 position : SV_Position) : SV_Target {
    uint x = uint(position.x);
    uint y = uint(position.y);
    uint row = flip != 0u ? height - 1u - y : y;
#if FORMAT == 0
    float luma = float(byte_at(row * stride + x));
    uint uv = uv_offset + (row >> 1u) * stride + (x & ~1u);
    return float4(rgb_from_yuv(luma, float(byte_at(uv)), float(byte_at(uv + 1u))), 1.0);
#elif FORMAT == 1
    // Y0 U Y1 V
    uint word = word_at(row * stride + (x & ~1u) * 2u);
    float luma = float((x & 1u) != 0u ? (word >> 16u) & 0xffu : word & 0xffu);
    return float4(rgb_from_yuv(luma, float((word >> 8u) & 0xffu), float(word >> 24u)), 1.0);
#elif FORMAT == 2
    // U Y0 V Y1
    uint word = word_at(row * stride + (x & ~1u) * 2u);
    float luma = float((x & 1u) != 0u ? word >> 24u : (word >> 8u) & 0xffu);
    return float4(rgb_from_yuv(luma, float(word & 0xffu), float((word >> 16u) & 0xffu)), 1.0);
#elif FORMAT == 3
    // B G R
    uint word = word_at(row * stride + x * 3u);
    float3 c = float3(float((word >> 16u) & 0xffu), float((word >> 8u) & 0xffu), float(word & 0xffu));
    return float4(rgb_from_codes(c), 1.0);
#else
    // B G R and a fourth byte that is ignored
    uint word = word_at(row * stride + x * 4u);
    float3 c = float3(float((word >> 16u) & 0xffu), float((word >> 8u) & 0xffu), float(word & 0xffu));
    return float4(rgb_from_codes(c), 1.0);
#endif
}

#else

#if FORMAT == 0
Texture2D<float> source_y : register(t0);
Texture2D<float2> source_uv : register(t1);
#else
Texture2D<float4> source : register(t0);
#endif

float4 ps(float4 position : SV_Position) : SV_Target {
    int2 p = int2(position.xy);
#if FORMAT == 0
    float luma = round(source_y.Load(int3(p, 0)) * 255.0);
    float2 uv = round(source_uv.Load(int3(p / 2, 0)) * 255.0);
    return float4(rgb_from_yuv(luma, uv.x, uv.y), 1.0);
#elif FORMAT == 1
    // One R8G8B8A8 texel holds Y0 U Y1 V for two pixels.
    float4 t = round(source.Load(int3(p.x / 2, p.y, 0)) * 255.0);
    float luma = (p.x & 1) != 0 ? t.z : t.x;
    return float4(rgb_from_yuv(luma, t.y, t.w), 1.0);
#else
    return float4(rgb_from_codes(source.Load(int3(p, 0)).rgb * 255.0), 1.0);
#endif
}

#endif
"#;

fn shader_format(format: CameraPixelFormat) -> u32 {
    match format {
        CameraPixelFormat::Nv12 => 0,
        CameraPixelFormat::Yuy2 => 1,
        CameraPixelFormat::Uyvy => 2,
        CameraPixelFormat::Rgb24 => 3,
        CameraPixelFormat::Rgb32 | CameraPixelFormat::Argb32 | CameraPixelFormat::Mjpg => 4,
    }
}

fn compile(source: u32, format: u32, entry: PCSTR, profile: PCSTR) -> Result<ID3DBlob> {
    let text = format!("#define SOURCE {source}\n#define FORMAT {format}\n{SHADER}");
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            text.as_ptr().cast(),
            text.len(),
            s!("camera.hlsl"),
            None,
            None::<&ID3DInclude>,
            entry,
            profile,
            D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let detail = errors
            .map(|blob| String::from_utf8_lossy(blob_bytes(&blob)).into_owned())
            .unwrap_or_default();
        return Err(error).with_context(|| format!("compile camera shader: {detail}"));
    }
    code.context("D3DCompile returned no camera shader bytecode")
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

/// A CPU payload on the GPU, as a raw 32-bit-word view.
struct Upload {
    buffer: ID3D11Buffer,
    capacity: usize,
    views: [Option<ID3D11ShaderResourceView>; 2],
}

/// Private sampleable copy of the visible region of a camera texture. The
/// camera's own texture may lack shader-resource binding.
struct Private {
    format: DXGI_FORMAT,
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
    views: [Option<ID3D11ShaderResourceView>; 2],
    count: usize,
}

/// Everything drawn on the shared immediate context. All state it needs is set
/// and cleared here; the context is never reset, since others use it too.
struct Draw {
    device: ID3D11Device5,
    vertex: ID3D11VertexShader,
    buffer_pixel: ID3D11PixelShader,
    texture_pixel: Option<ID3D11PixelShader>,
    constants: ID3D11Buffer,
    raster: ID3D11RasterizerState,
    blend: ID3D11BlendState,
    depth: ID3D11DepthStencilState,
    upload: Option<Upload>,
    copy: Option<Private>,
    // Render-target views of the output pool's few textures. The texture is
    // kept so its address cannot be reused for another resource.
    targets: HashMap<usize, (ID3D11Texture2D, ID3D11RenderTargetView)>,
}

impl Draw {
    fn new(device: &GraphicsDevice, layout: &Layout) -> Result<Self> {
        let device = device.raw().clone();
        let format = shader_format(layout.format);
        unsafe {
            let vertex_code = compile(SOURCE_BUFFER, 0, s!("vs"), s!("vs_5_0"))?;
            let mut vertex = None;
            device
                .CreateVertexShader(blob_bytes(&vertex_code), None, Some(&mut vertex))
                .context("create camera vertex shader")?;
            let pixel = |source: u32, format: u32| -> Result<ID3D11PixelShader> {
                let code = compile(source, format, s!("ps"), s!("ps_5_0"))?;
                let mut pixel = None;
                device
                    .CreatePixelShader(blob_bytes(&code), None, Some(&mut pixel))
                    .context("create camera pixel shader")?;
                pixel.context("D3D11 returned no camera pixel shader")
            };
            let buffer_pixel = pixel(SOURCE_BUFFER, format)?;
            let texture_pixel = match layout.format {
                CameraPixelFormat::Nv12
                | CameraPixelFormat::Yuy2
                | CameraPixelFormat::Rgb32
                | CameraPixelFormat::Argb32 => Some(pixel(SOURCE_TEXTURE, format)?),
                CameraPixelFormat::Uyvy | CameraPixelFormat::Rgb24 | CameraPixelFormat::Mjpg => {
                    None
                }
            };
            let mut constants = None;
            device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: size_of::<Params>() as u32,
                        Usage: D3D11_USAGE_DYNAMIC,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut constants),
                )
                .context("create camera constants")?;
            let mut raster = None;
            device
                .CreateRasterizerState(
                    &D3D11_RASTERIZER_DESC {
                        FillMode: D3D11_FILL_SOLID,
                        CullMode: D3D11_CULL_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    Some(&mut raster),
                )
                .context("create camera rasterizer state")?;
            let target = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: false.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_ZERO,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_ZERO,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device
                .CreateBlendState(
                    &D3D11_BLEND_DESC {
                        RenderTarget: [target; 8],
                        ..Default::default()
                    },
                    Some(&mut blend),
                )
                .context("create camera blend state")?;
            let keep = D3D11_DEPTH_STENCILOP_DESC {
                StencilFailOp: D3D11_STENCIL_OP_KEEP,
                StencilDepthFailOp: D3D11_STENCIL_OP_KEEP,
                StencilPassOp: D3D11_STENCIL_OP_KEEP,
                StencilFunc: D3D11_COMPARISON_ALWAYS,
            };
            let mut depth = None;
            device
                .CreateDepthStencilState(
                    &D3D11_DEPTH_STENCIL_DESC {
                        DepthEnable: false.into(),
                        DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
                        DepthFunc: D3D11_COMPARISON_ALWAYS,
                        StencilEnable: false.into(),
                        StencilReadMask: 0xff,
                        StencilWriteMask: 0xff,
                        FrontFace: keep,
                        BackFace: keep,
                    },
                    Some(&mut depth),
                )
                .context("create camera depth state")?;
            Ok(Self {
                vertex: vertex.context("D3D11 returned no camera vertex shader")?,
                buffer_pixel,
                texture_pixel,
                constants: constants.context("D3D11 returned no camera constants")?,
                raster: raster.context("D3D11 returned no camera rasterizer state")?,
                blend: blend.context("D3D11 returned no camera blend state")?,
                depth: depth.context("D3D11 returned no camera depth state")?,
                device,
                upload: None,
                copy: None,
                targets: HashMap::new(),
            })
        }
    }

    /// Make the payload view at least `bytes` (a multiple of four) long. Earlier
    /// pictures still reading the replaced buffer keep it alive until the GPU
    /// is done with them.
    fn ensure_upload(&mut self, bytes: usize) -> Result<()> {
        if self
            .upload
            .as_ref()
            .is_some_and(|upload| upload.capacity >= bytes)
        {
            return Ok(());
        }
        let byte_width = u32::try_from(bytes).context("camera.frame: payload size overflow")?;
        let mut buffer = None;
        let mut view = None;
        unsafe {
            self.device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: byte_width,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut buffer),
                )
                .with_context(|| format!("create {byte_width}-byte camera upload buffer"))?;
            let buffer: ID3D11Buffer = buffer.context("D3D11 returned no camera upload buffer")?;
            self.device
                .CreateShaderResourceView(
                    &buffer,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_TYPELESS,
                        ViewDimension: D3D11_SRV_DIMENSION_BUFFEREX,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            BufferEx: D3D11_BUFFEREX_SRV {
                                FirstElement: 0,
                                NumElements: byte_width / 4,
                                Flags: D3D11_BUFFEREX_SRV_FLAG_RAW.0 as u32,
                            },
                        },
                    }),
                    Some(&mut view),
                )
                .context("create camera upload view")?;
            self.upload = Some(Upload {
                buffer,
                capacity: bytes,
                views: [
                    Some(view.context("D3D11 returned no camera upload view")?),
                    None,
                ],
            });
        }
        Ok(())
    }

    /// Make the private copy texture match a camera texture's format.
    fn prepare_copy(&mut self, format: DXGI_FORMAT, layout: &Layout) -> Result<()> {
        if self.copy.as_ref().is_some_and(|copy| {
            copy.format == format && copy.width == layout.width && copy.height == layout.height
        }) {
            return Ok(());
        }
        let mut texture = None;
        unsafe {
            self.device
                .CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: layout.width,
                        Height: layout.height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )
                .with_context(|| {
                    format!(
                        "create private {format:?} camera texture; the GPU must be able to sample this format"
                    )
                })?;
        }
        let texture = texture.context("D3D11 returned no private camera texture")?;
        let (views, count) = if format == DXGI_FORMAT_NV12 {
            (
                [
                    Some(self.plane(&texture, DXGI_FORMAT_R8_UNORM, 0)?),
                    Some(self.plane(&texture, DXGI_FORMAT_R8G8_UNORM, 1)?),
                ],
                2,
            )
        } else {
            // YUY2 is read as R8G8B8A8, one texel per two pixels (Y0 U Y1 V).
            let view_format = if format == DXGI_FORMAT_YUY2 {
                DXGI_FORMAT_R8G8B8A8_UNORM
            } else {
                format
            };
            let mut view = None;
            unsafe {
                self.device.CreateShaderResourceView(
                    &texture,
                    Some(&D3D11_SHADER_RESOURCE_VIEW_DESC {
                        Format: view_format,
                        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: 1,
                            },
                        },
                    }),
                    Some(&mut view),
                )
            }
            .with_context(|| format!("create camera {format:?} view as {view_format:?}"))?;
            (
                [
                    Some(view.context("D3D11 returned no private camera view")?),
                    None,
                ],
                1,
            )
        };
        self.copy = Some(Private {
            format,
            width: layout.width,
            height: layout.height,
            texture,
            views,
            count,
        });
        Ok(())
    }

    fn plane(
        &self,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        plane: u32,
    ) -> Result<ID3D11ShaderResourceView> {
        let device: ID3D11Device3 = self
            .device
            .cast()
            .context("camera NV12 planes require ID3D11Device3")?;
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC1 {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC1_0 {
                Texture2D: D3D11_TEX2D_SRV1 {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    PlaneSlice: plane,
                },
            },
        };
        let mut view = None;
        unsafe { device.CreateShaderResourceView1(texture, Some(&desc), Some(&mut view)) }
            .with_context(|| format!("create camera NV12 plane {plane} view ({format:?})"))?;
        view.context("D3D11 returned no camera NV12 plane view")?
            .cast()
            .context("query camera NV12 plane view base interface")
    }

    fn target_view(&mut self, target: &ID3D11Texture2D) -> Result<ID3D11RenderTargetView> {
        let key = target.as_raw() as usize;
        if let Some((_, view)) = self.targets.get(&key) {
            return Ok(view.clone());
        }
        let mut view = None;
        unsafe {
            self.device
                .CreateRenderTargetView(target, None, Some(&mut view))
        }
        .context("create camera output view")?;
        let view = view.context("D3D11 returned no camera output view")?;
        self.targets.insert(key, (target.clone(), view.clone()));
        Ok(view)
    }

    /// Runs inside the output pool's render transaction: the shared context is
    /// locked and this picture's draw is ordered with every other use of it.
    fn run_buffer(
        &mut self,
        context: &ID3D11DeviceContext4,
        target: &ID3D11Texture2D,
        data: &[u8],
        params: &Params,
        layout: &Layout,
    ) -> Result<()> {
        let view = self.target_view(target)?;
        let upload = self
            .upload
            .as_ref()
            .context("camera upload buffer was not prepared")?;
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: data.len() as u32,
            bottom: 1,
            back: 1,
        };
        // The single CPU to GPU copy of this picture.
        unsafe {
            context.UpdateSubresource(&upload.buffer, 0, Some(&region), data.as_ptr().cast(), 0, 0)
        };
        self.draw(
            context,
            &view,
            &self.buffer_pixel,
            &upload.views[..1],
            params,
            layout,
        )
    }

    fn run_texture(
        &mut self,
        context: &ID3D11DeviceContext4,
        target: &ID3D11Texture2D,
        source: &ID3D11Texture2D,
        subresource: u32,
        params: &Params,
        layout: &Layout,
    ) -> Result<()> {
        let view = self.target_view(target)?;
        let copy = self
            .copy
            .as_ref()
            .context("camera copy texture was not prepared")?;
        let pixel = self.texture_pixel.as_ref().with_context(|| {
            format!(
                "camera.format: {} has no GPU shader path",
                layout.format.name()
            )
        })?;
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: layout.width,
            bottom: layout.height,
            back: 1,
        };
        // GPU to GPU, ordered with the draw below on this context.
        unsafe {
            context.CopySubresourceRegion(
                &copy.texture,
                0,
                0,
                0,
                0,
                source,
                subresource,
                Some(&region),
            )
        };
        self.draw(
            context,
            &view,
            pixel,
            &copy.views[..copy.count],
            params,
            layout,
        )
    }

    fn draw(
        &self,
        context: &ID3D11DeviceContext4,
        target: &ID3D11RenderTargetView,
        pixel: &ID3D11PixelShader,
        views: &[Option<ID3D11ShaderResourceView>],
        params: &Params,
        layout: &Layout,
    ) -> Result<()> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(
                    &self.constants,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .context("map camera constants")?;
            std::ptr::copy_nonoverlapping(
                params.as_ptr().cast::<u8>(),
                mapped.pData.cast::<u8>(),
                size_of::<Params>(),
            );
            context.Unmap(&self.constants, 0);

            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.HSSetShader(None::<&ID3D11HullShader>, None);
            context.DSSetShader(None::<&ID3D11DomainShader>, None);
            context.GSSetShader(None::<&ID3D11GeometryShader>, None);
            context.RSSetState(&self.raster);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: layout.width as f32,
                Height: layout.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.PSSetShader(pixel, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(views));
            context.OMSetBlendState(&self.blend, None, 0xffff_ffff);
            context.OMSetDepthStencilState(&self.depth, 0);
            context.OMSetRenderTargets(
                Some(&[Some(target.clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            context.Draw(3, 0);
            // Leave nothing bound that later users of this context could
            // trip over, such as a view of a buffer they want to write.
            context.OMSetRenderTargets(Some(&[None]), None::<&ID3D11DepthStencilView>);
            context.PSSetShaderResources(0, Some(&[None, None][..views.len()]));
            context.PSSetConstantBuffers(0, Some(&[None]));
            context.PSSetShader(None::<&ID3D11PixelShader>, None);
            context.VSSetShader(None::<&ID3D11VertexShader>, None);
        }
        Ok(())
    }
}
