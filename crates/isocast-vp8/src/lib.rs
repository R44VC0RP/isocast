//! VP8 video decoding with libvpx's VP8 decoder, vendored in `libvpx/` and
//! built statically (macOS has no VP8 decoder). Frames come out as 8-bit
//! 4:2:0 planes, which stay valid until the next decode.
//!
//! Only on arm64 macOS, the configuration that is vendored.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_void};
use std::fmt;
use std::mem::MaybeUninit;
use std::ptr;

mod ffi {
    use super::*;

    /// `vpx_codec_ctx_t`.
    #[repr(C)]
    pub struct Context {
        pub name: *const c_char,
        pub iface: *const c_void,
        pub err: c_int,
        pub err_detail: *const c_char,
        pub init_flags: c_long,
        pub config: *const c_void,
        pub private: *mut c_void,
    }

    /// `vpx_codec_dec_cfg_t`.
    #[repr(C)]
    pub struct DecoderConfig {
        pub threads: c_uint,
        pub w: c_uint,
        pub h: c_uint,
    }

    /// `vpx_image_t`.
    #[repr(C)]
    pub struct Image {
        pub fmt: c_int,
        pub cs: c_int,
        pub range: c_int,
        pub w: c_uint,
        pub h: c_uint,
        pub bit_depth: c_uint,
        pub d_w: c_uint,
        pub d_h: c_uint,
        pub r_w: c_uint,
        pub r_h: c_uint,
        pub x_chroma_shift: c_uint,
        pub y_chroma_shift: c_uint,
        pub planes: [*mut u8; 4],
        pub stride: [c_int; 4],
        pub bps: c_int,
        pub user_priv: *mut c_void,
        pub img_data: *mut u8,
        pub img_data_owner: c_int,
        pub self_allocd: c_int,
        pub fb_priv: *mut c_void,
    }

    /// `VPX_DECODER_ABI_VERSION` (3 + VPX_CODEC_ABI_VERSION, which is 4 +
    /// VPX_IMAGE_ABI_VERSION, 5) in the vendored headers.
    pub const DECODER_ABI_VERSION: c_int = 12;
    /// `VPX_IMG_FMT_I420`.
    pub const IMG_FMT_I420: c_int = 0x100 | 2;
    /// `VPX_CR_FULL_RANGE`.
    pub const CR_FULL_RANGE: c_int = 1;

    unsafe extern "C" {
        pub fn vpx_codec_vp8_dx() -> *const c_void;
        pub fn vpx_codec_dec_init_ver(
            ctx: *mut Context,
            iface: *const c_void,
            cfg: *const DecoderConfig,
            flags: c_long,
            ver: c_int,
        ) -> c_int;
        pub fn vpx_codec_decode(
            ctx: *mut Context,
            data: *const u8,
            data_sz: c_uint,
            user_priv: *mut c_void,
            deadline: c_long,
        ) -> c_int;
        pub fn vpx_codec_get_frame(ctx: *mut Context, iter: *mut *const c_void) -> *mut Image;
        pub fn vpx_codec_destroy(ctx: *mut Context) -> c_int;
        pub fn vpx_codec_error(ctx: *const Context) -> *const c_char;
        pub fn vpx_codec_error_detail(ctx: *const Context) -> *const c_char;
    }
}

/// A libvpx error, with its message.
#[derive(Debug, Clone)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// One plane of a decoded frame.
#[derive(Clone, Copy)]
pub struct Plane<'a> {
    /// `stride * (height - 1) + width` bytes from the top-left sample.
    pub data: &'a [u8],
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

impl<'a> Plane<'a> {
    /// Row `y`'s samples.
    pub fn row(&self, y: usize) -> &'a [u8] {
        &self.data[y * self.stride..y * self.stride + self.width]
    }
}

/// A decoded frame: 8-bit 4:2:0.
pub struct Frame<'a> {
    pub width: usize,
    pub height: usize,
    pub y: Plane<'a>,
    pub u: Plane<'a>,
    pub v: Plane<'a>,
    /// Whether the stream says its samples are full range (VP8's own
    /// streams are video range).
    pub full_range: bool,
}

/// A VP8 decoder.
pub struct Decoder {
    // Boxed: libvpx keeps no pointer to it, but it is never moved anyway.
    context: Box<ffi::Context>,
}

// SAFETY: a libvpx decoder may be used from any thread, one at a time
// (`&mut self` for every call).
unsafe impl Send for Decoder {}

impl Decoder {
    /// A decoder using up to `threads` threads (libvpx uses more than one
    /// only for streams with several token partitions).
    pub fn new(threads: u32) -> Result<Self, Error> {
        let config = ffi::DecoderConfig {
            threads: threads.max(1),
            w: 0,
            h: 0,
        };
        // SAFETY: init fills the zeroed context; it's destroyed on drop
        // only if init succeeded.
        let mut context: Box<ffi::Context> =
            Box::new(unsafe { MaybeUninit::zeroed().assume_init() });
        let status = unsafe {
            ffi::vpx_codec_dec_init_ver(
                &mut *context,
                ffi::vpx_codec_vp8_dx(),
                &config,
                0,
                ffi::DECODER_ABI_VERSION,
            )
        };
        if status != 0 {
            return Err(Error(format!(
                "could not start the VP8 decoder (libvpx error {status})"
            )));
        }
        Ok(Self { context })
    }

    /// Decode one frame and return the picture it shows, if any (a VP8
    /// frame may be a hidden reference frame, which shows nothing).
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Frame<'_>>, Error> {
        let len = u32::try_from(data.len()).map_err(|_| Error("a VP8 frame over 4 GB".into()))?;
        if len == 0 {
            // An empty frame means "flush" to libvpx; VP8 holds no frames.
            return Ok(None);
        }
        // SAFETY: the context is initialised; libvpx reads `len` bytes.
        let status = unsafe {
            ffi::vpx_codec_decode(&mut *self.context, data.as_ptr(), len, ptr::null_mut(), 0)
        };
        if status != 0 {
            return Err(self.error(status));
        }
        let mut iter = ptr::null();
        let mut last = ptr::null_mut();
        loop {
            // SAFETY: as above; images stay valid until the next decode.
            let image = unsafe { ffi::vpx_codec_get_frame(&mut *self.context, &mut iter) };
            if image.is_null() {
                break;
            }
            last = image;
        }
        if last.is_null() {
            return Ok(None);
        }
        // SAFETY: a non-null image from get_frame, valid while `self` is
        // borrowed (until the next decode or drop).
        let image = unsafe { &*last };
        if image.fmt != ffi::IMG_FMT_I420
            || image.bit_depth != 8
            || image.x_chroma_shift != 1
            || image.y_chroma_shift != 1
        {
            return Err(Error(format!(
                "libvpx gave an unexpected picture format ({:#x})",
                image.fmt
            )));
        }
        let (width, height) = (image.d_w as usize, image.d_h as usize);
        let plane = |index: usize, width: usize, height: usize| -> Result<Plane<'_>, Error> {
            let stride = image.stride[index];
            let base = image.planes[index];
            if base.is_null() || stride < width as c_int || width == 0 || height == 0 {
                return Err(Error("libvpx gave an empty picture".into()));
            }
            let stride = stride as usize;
            let len = stride * (height - 1) + width;
            // SAFETY: libvpx's planes hold `stride` bytes per row for
            // `height` rows (the last row at least `width`).
            let data = unsafe { std::slice::from_raw_parts(base, len) };
            Ok(Plane {
                data,
                stride,
                width,
                height,
            })
        };
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        Ok(Some(Frame {
            width,
            height,
            y: plane(0, width, height)?,
            u: plane(1, chroma_width, chroma_height)?,
            v: plane(2, chroma_width, chroma_height)?,
            full_range: image.range == ffi::CR_FULL_RANGE,
        }))
    }

    fn error(&self, status: c_int) -> Error {
        // SAFETY: libvpx returns static or context-owned C strings.
        let text = |pointer: *const c_char| {
            (!pointer.is_null()).then(|| {
                unsafe { CStr::from_ptr(pointer) }
                    .to_string_lossy()
                    .into_owned()
            })
        };
        let message = text(unsafe { ffi::vpx_codec_error(&*self.context) });
        let detail = text(unsafe { ffi::vpx_codec_error_detail(&*self.context) });
        let mut out = format!("libvpx error {status}");
        if let Some(message) = message {
            out = format!("{message} ({out})");
        }
        if let Some(detail) = detail {
            out.push_str(&format!(": {detail}"));
        }
        Error(out)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: initialised in `new`, destroyed once.
        unsafe { ffi::vpx_codec_destroy(&mut *self.context) };
    }
}
