//! IOSurfaces, handed between processes as Mach ports.
//!
//! The daemon checks a Mach service in with the bootstrap server under a
//! name of its own (`dev.isocast.isocastd.<pid>.surfaces`): launchd lets any
//! process register a name nobody holds in its own bootstrap namespace (the
//! user's login session), and forgets it when the process exits. A client
//! sends the service a request holding a token and a reply port; the reply
//! carries one send right per surface (`IOSurfaceCreateMachPort`), which
//! the client turns back into surfaces (`IOSurfaceLookupFromMachPort`).

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::mem::{size_of, zeroed};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mach2::bootstrap::{bootstrap_check_in, bootstrap_look_up, bootstrap_port, bootstrap_strerror};
use mach2::kern_return::{KERN_SUCCESS, kern_return_t};
use mach2::mach_port::{mach_port_allocate, mach_port_deallocate, mach_port_mod_refs};
use mach2::mach_time::{mach_absolute_time, mach_timebase_info, mach_timebase_info_data_t};
use mach2::message::{
    MACH_MSG_PORT_DESCRIPTOR, MACH_MSG_SUCCESS, MACH_MSG_TIMEOUT_NONE, MACH_MSG_TYPE_COPY_SEND,
    MACH_MSG_TYPE_MAKE_SEND_ONCE, MACH_MSG_TYPE_MOVE_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE,
    MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX, MACH_RCV_MSG, MACH_RCV_TIMED_OUT, MACH_RCV_TIMEOUT,
    MACH_SEND_INVALID_DEST, MACH_SEND_MSG, MACH_SEND_TIMED_OUT, MACH_SEND_TIMEOUT, mach_msg,
    mach_msg_body_t, mach_msg_destroy, mach_msg_header_t, mach_msg_port_descriptor_t,
    mach_msg_return_t, mach_msg_trailer_t,
};
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use mach2::traps::mach_task_self;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_io_surface::{
    IOSurfaceRef, kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceHeight,
    kIOSurfacePixelFormat, kIOSurfaceWidth,
};

use crate::{MAX_SLOTS, Token};

/// `msgh_id`s of the service's messages.
const REQUEST: i32 = 0x1500_ca51;
const REPLY: i32 = REQUEST + 1;
/// The token is unknown or was used already.
const REFUSED: i32 = REQUEST + 2;

/// How long the daemon waits to hand a reply to a client.
const REPLY_TIMEOUT_MS: u32 = 1000;

/// An 8-bit RGBA picture in GPU memory that several processes can use at
/// once. Cloning shares the same surface.
#[derive(Clone)]
pub struct Surface(
    CFRetained<IOSurfaceRef>,
    #[allow(dead_code)] Arc<SurfaceLease>,
);

struct SurfaceLease(u32);

type SurfaceCounts = HashMap<u32, (usize, u64)>;
fn surface_counts() -> &'static Mutex<SurfaceCounts> {
    static COUNTS: OnceLock<Mutex<SurfaceCounts>> = OnceLock::new();
    COUNTS.get_or_init(Mutex::default)
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        let mut counts = surface_counts().lock().unwrap_or_else(|e| e.into_inner());
        let (refs, _) = counts.get_mut(&self.0).expect("live surface");
        *refs -= 1;
        if *refs == 0 {
            counts.remove(&self.0);
        }
    }
}

/// Distinct shared surfaces retained in this process, including GPU texture
/// wrappers and pending Mach offers. A client may retain its own copies after
/// disconnecting; those cannot pin a recorder pixel-buffer pool.
pub fn retained_surfaces() -> (usize, u64) {
    let counts = surface_counts().lock().unwrap_or_else(|e| e.into_inner());
    (counts.len(), counts.values().map(|(_, bytes)| *bytes).sum())
}

// SAFETY: IOSurfaces are reference counted atomically and may be used from
// any thread; their pixels are only touched by the GPU here.
unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    /// Keep an existing surface alive for a Mach handoff. Its pixel format
    /// is unchanged; `texture` is only for RGBA surfaces, not camera BGRA.
    pub fn from_io_surface(surface: CFRetained<IOSurfaceRef>) -> Self {
        let id = surface.id();
        let bytes = (surface.bytes_per_row() as u64).saturating_mul(surface.height() as u64);
        let mut counts = surface_counts().lock().unwrap_or_else(|e| e.into_inner());
        counts.entry(id).or_insert((0, bytes)).0 += 1;
        Self(surface, Arc::new(SurfaceLease(id)))
    }

    /// A new `width`×`height` surface of 8-bit RGBA (`Rgba8Unorm` on the
    /// GPU), its contents undefined.
    pub fn rgba(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("a surface can't be {width}×{height}");
        }
        let row =
            IOSurfaceRef::align_property(unsafe { kIOSurfaceBytesPerRow }, width as usize * 4);
        let numbers = [
            CFNumber::new_i32(width as i32),
            CFNumber::new_i32(height as i32),
            CFNumber::new_i32(4),
            CFNumber::new_i64(row as i64),
            CFNumber::new_i32(u32::from_be_bytes(*b"RGBA") as i32),
        ];
        let keys = unsafe {
            [
                kIOSurfaceWidth,
                kIOSurfaceHeight,
                kIOSurfaceBytesPerElement,
                kIOSurfaceBytesPerRow,
                kIOSurfacePixelFormat,
            ]
        };
        let values: Vec<&CFNumber> = numbers.iter().map(|n| &**n).collect();
        let properties = CFDictionary::<CFString, CFNumber>::from_slices(&keys, &values);
        let surface = unsafe { IOSurfaceRef::new(properties.as_opaque()) }
            .with_context(|| format!("could not create a {width}×{height} IOSurface"))?;
        Ok(Self::from_io_surface(surface))
    }

    pub fn width(&self) -> u32 {
        self.0.width() as u32
    }

    pub fn height(&self) -> u32 {
        self.0.height() as u32
    }

    /// The surface's system-wide id (for logs; it is not a way to share it).
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// The `IOSurfaceRef` itself.
    pub fn io_surface(&self) -> &IOSurfaceRef {
        &self.0
    }

    /// The surface as an `Rgba8Unorm` texture of `device` (a Metal device)
    /// with `usage`, in place: the GPU reads and writes the surface's own
    /// memory. The texture keeps the surface alive. For Slint's
    /// `Image::try_from(wgpu::Texture)` ask for `TEXTURE_BINDING |
    /// RENDER_ATTACHMENT`.
    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        device: &wgpu::Device,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        use objc2_metal::{
            MTLDevice, MTLPixelFormat, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
        };
        use wgpu::hal;

        let metal = {
            let device = unsafe { device.as_hal::<hal::api::Metal>() }
                .context("the GPU is not a Metal device")?;
            device.raw_device().clone()
        };
        let (width, height) = (self.width(), self.height());
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                width as usize,
                height as usize,
                false,
            )
        };
        let mut metal_usage = MTLTextureUsage::Unknown;
        if usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
            metal_usage |= MTLTextureUsage::ShaderRead;
        }
        if usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
            metal_usage |= MTLTextureUsage::RenderTarget;
        }
        if usage.contains(wgpu::TextureUsages::STORAGE_BINDING) {
            metal_usage |= MTLTextureUsage::ShaderWrite;
        }
        descriptor.setUsage(metal_usage);
        let raw = metal
            .newTextureWithDescriptor_iosurface_plane(&descriptor, &self.0, 0)
            .context("Metal could not make a texture of the surface")?;
        let keep = self.clone();
        let hal_texture = unsafe {
            hal::metal::Device::texture_from_raw(
                raw,
                wgpu::TextureFormat::Rgba8Unorm,
                MTLTextureType::Type2D,
                1,
                1,
                hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                Some(Box::new(move || drop(keep))),
            )
        };
        let state = if usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
            wgpu::TextureUses::RESOURCE
        } else {
            wgpu::TextureUses::COLOR_TARGET
        };
        Ok(unsafe {
            device.create_texture_from_hal::<hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("shared surface"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage,
                    view_formats: &[],
                },
                state,
            )
        })
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Surface({} {}×{})",
            self.id(),
            self.width(),
            self.height()
        )
    }
}

/// The host clock now, in nanoseconds: the clock frame times are on.
pub fn host_now() -> u64 {
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = mach_timebase_info_data_t { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut info) };
        (u64::from(info.numer.max(1)), u64::from(info.denom.max(1)))
    });
    let ticks = unsafe { mach_absolute_time() };
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

/// The daemon's side: a Mach service that hands each offered ring of
/// surfaces to the client holding its token, once.
pub struct Exporter {
    service: String,
    offers: Arc<Offers>,
}

/// Rings offered and not fetched yet, by token.
type Offers = Mutex<HashMap<Token, Vec<Surface>>>;

/// A request: the token, nothing else.
#[repr(C)]
struct Request {
    header: mach_msg_header_t,
    token: [u8; 16],
}

/// Room for a request and the trailer the kernel appends.
#[repr(C)]
struct RequestBuffer {
    request: Request,
    trailer: mach_msg_trailer_t,
    spare: [u8; 64],
}

/// A reply: a send right for each surface.
#[repr(C)]
struct Reply {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    ports: [mach_msg_port_descriptor_t; MAX_SLOTS],
}

#[repr(C)]
struct ReplyBuffer {
    reply: Reply,
    trailer: mach_msg_trailer_t,
    spare: [u8; 64],
}

impl Exporter {
    /// This process's exporter, its service registered on first use.
    pub fn shared() -> Result<&'static Exporter> {
        static SHARED: OnceLock<Result<Exporter, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| Exporter::start().map_err(|err| format!("{err:#}")))
            .as_ref()
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    fn start() -> Result<Exporter> {
        let service = format!("dev.isocast.isocastd.{}.surfaces", std::process::id());
        let name = CString::new(service.clone())?;
        let mut port: mach_port_t = MACH_PORT_NULL;
        let status = unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &mut port) };
        if status != KERN_SUCCESS {
            bail!(
                "could not register the Mach service {service}: {}",
                bootstrap_error(status)
            );
        }
        let offers = Arc::new(Offers::default());
        let served = offers.clone();
        thread::Builder::new()
            .name("shared surfaces".into())
            .spawn(move || serve(port, &served))
            .context("could not start the shared surface service")?;
        Ok(Exporter { service, offers })
    }

    /// The Mach service's bootstrap name, which clients look up.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Offer `surfaces` (at most [`MAX_SLOTS`]) to whoever brings the token
    /// returned, once.
    pub fn offer(&self, surfaces: Vec<Surface>) -> Token {
        assert!(surfaces.len() <= MAX_SLOTS, "too many surfaces in a ring");
        let token = Token::random();
        self.offers.lock().unwrap().insert(token, surfaces);
        token
    }

    /// Take back an offer nobody fetched.
    pub fn withdraw(&self, token: Token) {
        self.offers.lock().unwrap().remove(&token);
    }
}

/// Answer requests on `port` for as long as the process runs.
fn serve(port: mach_port_t, offers: &Offers) {
    loop {
        let mut buffer: RequestBuffer = unsafe { zeroed() };
        let status = unsafe {
            mach_msg(
                &mut buffer.request.header,
                MACH_RCV_MSG,
                0,
                size_of::<RequestBuffer>() as u32,
                port,
                MACH_MSG_TIMEOUT_NONE,
                MACH_PORT_NULL,
            )
        };
        if status != MACH_MSG_SUCCESS {
            // A message too large is destroyed by the kernel; keep going.
            continue;
        }
        let header = buffer.request.header;
        let complex = header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0;
        if complex
            || header.msgh_id != REQUEST
            || header.msgh_size as usize != size_of::<Request>()
            || header.msgh_remote_port == MACH_PORT_NULL
        {
            // Not ours: let go of any rights it carried.
            unsafe { mach_msg_destroy(&mut buffer.request.header) };
            continue;
        }
        let surfaces = offers.lock().unwrap().remove(&Token(buffer.request.token));
        reply(header.msgh_remote_port, surfaces.as_deref());
    }
}

/// Send `surfaces` (or a refusal) to the send-once right `to`.
fn reply(to: mach_port_t, surfaces: Option<&[Surface]>) {
    let mut message: Reply = unsafe { zeroed() };
    let count = surfaces.map_or(0, <[Surface]>::len);
    message.header.msgh_remote_port = to;
    message.header.msgh_local_port = MACH_PORT_NULL;
    match surfaces {
        Some(surfaces) => {
            message.header.msgh_bits =
                MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0) | MACH_MSGH_BITS_COMPLEX;
            message.header.msgh_id = REPLY;
            message.header.msgh_size = (size_of::<mach_msg_header_t>()
                + size_of::<mach_msg_body_t>()
                + count * size_of::<mach_msg_port_descriptor_t>())
                as u32;
            message.body.msgh_descriptor_count = count as u32;
            for (descriptor, surface) in message.ports.iter_mut().zip(surfaces) {
                descriptor.name = surface.0.create_mach_port();
                descriptor.disposition = MACH_MSG_TYPE_MOVE_SEND as u8;
                descriptor.type_ = MACH_MSG_PORT_DESCRIPTOR as u8;
            }
        }
        None => {
            message.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
            message.header.msgh_id = REFUSED;
            message.header.msgh_size = size_of::<mach_msg_header_t>() as u32;
        }
    }
    let status = unsafe {
        mach_msg(
            &mut message.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            message.header.msgh_size,
            0,
            MACH_PORT_NULL,
            REPLY_TIMEOUT_MS,
            MACH_PORT_NULL,
        )
    };
    // These leave the message's rights with the sender.
    if status == MACH_SEND_INVALID_DEST || status == MACH_SEND_TIMED_OUT {
        for descriptor in &message.ports[..count] {
            unsafe { mach_port_deallocate(mach_task_self(), descriptor.name) };
        }
        unsafe { mach_port_deallocate(mach_task_self(), to) };
    }
}

/// The client's side: fetch the surfaces offered under `token` from the
/// daemon's Mach service `service`, waiting up to `timeout`.
pub fn fetch(service: &str, token: Token, timeout: Duration) -> Result<Vec<Surface>> {
    let name = CString::new(service).context("service: not a valid name")?;
    let mut server: mach_port_t = MACH_PORT_NULL;
    let status = unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut server) };
    if status != KERN_SUCCESS {
        bail!(
            "could not find isocastd's surface service {service} ({}); the daemon may have \
             stopped, or run in another login session (for example started over SSH)",
            bootstrap_error(status)
        );
    }
    let reply_port = ReplyPort::new()?;
    let result = (|| {
        let mut request = Request {
            header: mach_msg_header_t {
                msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE),
                msgh_size: size_of::<Request>() as u32,
                msgh_remote_port: server,
                msgh_local_port: reply_port.0,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: REQUEST,
            },
            token: token.0,
        };
        let millis = timeout.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
        let status = unsafe {
            mach_msg(
                &mut request.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                size_of::<Request>() as u32,
                0,
                MACH_PORT_NULL,
                millis,
                MACH_PORT_NULL,
            )
        };
        check_msg(status, "send a request to isocastd's surface service")?;
        let mut buffer: ReplyBuffer = unsafe { zeroed() };
        let status = unsafe {
            mach_msg(
                &mut buffer.reply.header,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                0,
                size_of::<ReplyBuffer>() as u32,
                reply_port.0,
                millis,
                MACH_PORT_NULL,
            )
        };
        if status == MACH_RCV_TIMED_OUT {
            bail!("isocastd's surface service did not answer within {timeout:?}");
        }
        check_msg(status, "receive surfaces from isocastd")?;
        let header = buffer.reply.header;
        let complex = header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0;
        if header.msgh_id == REFUSED {
            bail!("isocastd refused the token: it was used already or the ring was replaced");
        }
        let count = buffer.reply.body.msgh_descriptor_count as usize;
        if header.msgh_id != REPLY || !complex || count > MAX_SLOTS {
            unsafe { mach_msg_destroy(&mut buffer.reply.header) };
            bail!("an unexpected reply from isocastd's surface service");
        }
        let mut surfaces = Vec::with_capacity(count);
        let mut failed = false;
        for descriptor in &buffer.reply.ports[..count] {
            let port = descriptor.name;
            if descriptor.type_ as u32 == MACH_MSG_PORT_DESCRIPTOR && port != MACH_PORT_NULL {
                match IOSurfaceRef::lookup_from_mach_port(port) {
                    Some(surface) => surfaces.push(Surface::from_io_surface(surface)),
                    None => failed = true,
                }
                unsafe { mach_port_deallocate(mach_task_self(), port) };
            } else {
                failed = true;
            }
        }
        if failed {
            bail!("isocastd sent a surface this process could not open");
        }
        Ok(surfaces)
    })();
    unsafe { mach_port_deallocate(mach_task_self(), server) };
    result
}

/// A receive right for one reply, destroyed when dropped.
struct ReplyPort(mach_port_t);

impl ReplyPort {
    fn new() -> Result<Self> {
        let mut port = MACH_PORT_NULL;
        let status =
            unsafe { mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port) };
        if status != KERN_SUCCESS {
            bail!("could not allocate a Mach port (error {status:#x})");
        }
        Ok(Self(port))
    }
}

impl Drop for ReplyPort {
    fn drop(&mut self) {
        unsafe { mach_port_mod_refs(mach_task_self(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

fn check_msg(status: mach_msg_return_t, doing: &str) -> Result<()> {
    if status != MACH_MSG_SUCCESS {
        bail!("could not {doing} (Mach error {status:#x})");
    }
    Ok(())
}

fn bootstrap_error(status: kern_return_t) -> String {
    let text = unsafe { bootstrap_strerror(status) };
    if text.is_null() {
        return format!("error {status}");
    }
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}
