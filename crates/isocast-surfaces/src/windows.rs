//! Same-user, same-session named NT allocations. Only small capabilities and
//! geometry cross the authenticated pipe; pixels stay on the adapter. Names
//! are never reused, so a delayed fetch cannot duplicate a recycled HANDLE.

use std::collections::HashMap;
use std::io::{Read, Write};
#[cfg(feature = "wgpu")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "wgpu")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
#[cfg(feature = "wgpu")]
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
#[cfg(feature = "wgpu")]
use windows::Win32::Security::Authorization::*;
#[cfg(feature = "wgpu")]
use windows::Win32::Security::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::*;
#[cfg(feature = "wgpu")]
use windows::core::{GUID, IUnknown, PWSTR, implement};
use windows::core::{Owned, PCWSTR};

use crate::{MAX_SLOTS, Token};

const MAGIC: &[u8; 8] = b"ISOCDX01";
const MAX_BYTES: u64 = 512 * 1024 * 1024;
static BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// One adapter identity for every native device in this process. The daemon
/// chooses among wgpu-compatible DX12 adapters, not DXGI's unfiltered first
/// adapter. Clients supply the daemon's LUID before creating their renderer.
/// Keep the identity across device loss: switching GPUs would invalidate live
/// capture devices and other processes' shared allocations.
#[cfg(feature = "wgpu")]
pub fn dx12_adapter(
    instance: &wgpu::Instance,
    requested: Option<(u32, i32)>,
) -> Result<wgpu::Adapter> {
    static ADAPTER: Mutex<Option<(u32, i32)>> = Mutex::new(None);
    let mut selected = ADAPTER.lock().unwrap();
    if let (Some(current), Some(requested)) = (*selected, requested) {
        ensure!(
            current == requested,
            "daemon adapter {requested:?} differs from this process's adapter {current:?}; restart the client"
        );
    }
    let adapter = match requested.or(*selected) {
        Some(luid) => block_on(instance.enumerate_adapters(wgpu::Backends::DX12))
            .into_iter()
            .find(|adapter| dx12_adapter_luid(adapter).ok() == Some(luid))
            .with_context(|| format!("DX12 adapter {luid:?} is unavailable; restart Isocast"))?,
        None => block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no compatible DX12 adapter found")?,
    };
    *selected = Some(dx12_adapter_luid(&adapter)?);
    Ok(adapter)
}

#[cfg(feature = "wgpu")]
pub fn dx12_adapter_luid(adapter: &wgpu::Adapter) -> Result<(u32, i32)> {
    let raw = unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }
        .context("Windows graphics require a DX12 adapter")?;
    let desc = unsafe { raw.raw_adapter().GetDesc2() }?;
    Ok((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
}

/// Native wgpu adapter requests complete synchronously.
#[cfg(feature = "wgpu")]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

struct Handle(#[cfg_attr(not(feature = "wgpu"), allow(dead_code))] Owned<HANDLE>);
// NT handles are process-owned, not apartment-affine. Arc owns their closure.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

struct Allocation {
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    handle: Handle,
    name: Token,
    width: u32,
    height: u32,
    luid: (u32, i32),
    _budget: Budget,
}

struct Budget(u64);
impl Budget {
    fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "shared surface dimensions must be 1..=8192"
        );
        // Conservative RGBA tile footprint, including narrow/tiny surfaces.
        // Count allocations too: resize floods must not evade the byte bound.
        let bytes =
            u64::from(width.div_ceil(256)) * u64::from(height.div_ceil(256)) * 256 * 256 * 4;
        ALLOCATIONS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 128).then_some(count + 1)
            })
            .map_err(|_| anyhow::anyhow!("shared previews exceed 128 retained allocations"))?;
        if BYTES
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= MAX_BYTES)
            })
            .is_err()
        {
            ALLOCATIONS.fetch_sub(1, Ordering::AcqRel);
            bail!("shared previews exceed the 512 MiB estimated process budget; close a viewer");
        }
        Ok(Self(bytes))
    }
}
impl Drop for Budget {
    fn drop(&mut self) {
        BYTES.fetch_sub(self.0, Ordering::AcqRel);
        ALLOCATIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct Surface(Arc<Allocation>);
impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("adapter", &self.0.luid)
            .finish()
    }
}

fn name(token: Token) -> Vec<u16> {
    format!("Local\\Isocast.Surface.{token}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}
fn session() -> Result<u32> {
    let mut id = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) }?;
    Ok(id)
}

#[cfg(feature = "wgpu")]
struct Security(PSECURITY_DESCRIPTOR);
#[cfg(feature = "wgpu")]
impl Security {
    fn owner() -> Result<Self> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
            let token = Owned::new(token);
            let mut len = 0;
            let _ = GetTokenInformation(*token, TokenUser, None, 0, &mut len);
            ensure!(len > 0 && len < 65536, "invalid user token size");
            let mut data = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
            GetTokenInformation(
                *token,
                TokenUser,
                Some(data.as_mut_ptr().cast()),
                len,
                &mut len,
            )?;
            let user = &*data.as_ptr().cast::<TOKEN_USER>();
            let mut sid = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
            let text = sid.to_string();
            LocalFree(Some(HLOCAL(sid.0.cast())));
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", text?)
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
            Ok(Self(descriptor))
        }
    }
}
#[cfg(feature = "wgpu")]
impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

impl Surface {
    pub fn rgba(_width: u32, _height: u32) -> Result<Self> {
        bail!("Windows shared surfaces require rgba_on with the producer's DX12 device")
    }
    pub fn width(&self) -> u32 {
        self.0.width
    }
    pub fn height(&self) -> u32 {
        self.0.height
    }
    pub fn id(&self) -> u32 {
        u32::from_le_bytes(self.0.name.0[..4].try_into().unwrap())
    }

    #[cfg(feature = "wgpu")]
    pub fn rgba_on(device: &wgpu::Device, width: u32, height: u32) -> Result<Self> {
        let budget = Budget::new(width, height)?;
        let raw = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
            .context("shared previews require a DX12 wgpu device")?
            .raw_device()
            .clone();
        let luid = unsafe { raw.GetAdapterLuid() };
        let mut resource: Option<ID3D12Resource> = None;
        unsafe {
            raw.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    CreationNodeMask: 1,
                    VisibleNodeMask: 1,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_SHARED,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: u64::from(width),
                    Height: height,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                        | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
        }?;
        let resource = resource.context("DX12 returned no preview allocation")?;
        let token = Token::random();
        let name = name(token);
        let security = Security::owner()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0.0,
            bInheritHandle: false.into(),
        };
        let handle = unsafe {
            raw.CreateSharedHandle(
                &resource,
                Some(&attributes),
                GENERIC_ALL.0,
                PCWSTR(name.as_ptr()),
            )
        }?;
        Ok(Self(Arc::new(Allocation {
            handle: Handle(unsafe { Owned::new(handle) }),
            name: token,
            width,
            height,
            luid: (luid.LowPart, luid.HighPart),
            _budget: budget,
        })))
    }

    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        device: &wgpu::Device,
        usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        let raw = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
            .context("shared previews require DX12 (no CPU/cross-adapter fallback)")?
            .raw_device()
            .clone();
        let luid = unsafe { raw.GetAdapterLuid() };
        ensure!(
            (luid.LowPart, luid.HighPart) == self.0.luid,
            "shared preview adapter mismatch; app and daemon must select the same GPU"
        );
        let mut resource: Option<ID3D12Resource> = None;
        unsafe { raw.OpenSharedHandle(*self.0.handle.0, &mut resource) }?;
        let resource = resource.context("DX12 returned no shared preview resource")?;
        let desc = unsafe { resource.GetDesc() };
        ensure!(
            desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
                && desc.Width == u64::from(self.width())
                && desc.Height == self.height()
                && desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM
                && desc.DepthOrArraySize == 1
                && desc.MipLevels == 1
                && desc.SampleDesc.Count == 1
                && desc.SampleDesc.Quality == 0
                && desc.Flags.contains(
                    D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS
                        | D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                ),
            "shared preview resource does not match its RGBA8 descriptor"
        );
        let lease: IUnknown = Lease {
            _surface: self.clone(),
        }
        .into();
        unsafe { resource.SetPrivateDataInterface(&LEASE_ID, &lease) }?;
        let size = wgpu::Extent3d {
            width: self.width(),
            height: self.height(),
            depth_or_array_layers: 1,
        };
        let texture = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
                wgpu::TextureFormat::Rgba8Unorm,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            )
        };
        // Simultaneous-access resources decay to COMMON after execution. The
        // producer announces only completed GPU writes; consumer acknowledgments
        // follow a fence on the real rendering queue before any slot reuse.
        Ok(unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                texture,
                &wgpu::TextureDescriptor {
                    label: Some("shared preview"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage,
                    view_formats: &[],
                },
                wgpu::TextureUses::PRESENT,
            )
        })
    }
}

#[cfg(feature = "wgpu")]
const LEASE_ID: GUID = GUID::from_u128(0xf9a878fa_1b40_4fd7_bbf6_f7c056f753b5);
#[cfg(feature = "wgpu")]
#[implement()]
struct Lease {
    _surface: Surface,
}

pub fn host_now() -> u64 {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut f = 0;
        unsafe { QueryPerformanceFrequency(&mut f) }.expect("QPC frequency");
        f
    });
    let mut now = 0;
    unsafe { QueryPerformanceCounter(&mut now) }.expect("QPC counter");
    (i128::from(now) * 1_000_000_000 / i128::from(frequency)).max(0) as u64
}

type Offers = Mutex<HashMap<Token, Vec<Surface>>>;
pub struct Exporter {
    service: String,
    offers: Arc<Offers>,
}
impl Exporter {
    pub fn shared() -> Result<&'static Self> {
        static EXPORTER: OnceLock<Result<Exporter, String>> = OnceLock::new();
        EXPORTER
            .get_or_init(|| Self::start().map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
    fn start() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "isocast-surfaces-{}-{}",
            std::process::id(),
            Token::random()
        ));
        let listener = isocast_ipc::Listener::bind(&path)?;
        let offers = Arc::new(Offers::default());
        let serve = offers.clone();
        let active = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("surface handoff".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    if active.fetch_add(1, Ordering::AcqRel) >= 32 {
                        active.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let served = serve.clone();
                    let count = active.clone();
                    if std::thread::Builder::new()
                        .name("surface fetch".into())
                        .spawn(move || {
                            let _ = serve_ring(stream, &served);
                            count.fetch_sub(1, Ordering::AcqRel);
                        })
                        .is_err()
                    {
                        active.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            })?;
        Ok(Self {
            service: path.to_string_lossy().into_owned(),
            offers,
        })
    }
    pub fn service(&self) -> &str {
        &self.service
    }
    pub fn offer(&self, surfaces: Vec<Surface>) -> Token {
        assert!(surfaces.len() <= MAX_SLOTS);
        let token = Token::random();
        self.offers.lock().unwrap().insert(token, surfaces);
        token
    }
    pub fn withdraw(&self, token: Token) {
        self.offers.lock().unwrap().remove(&token);
    }
}

fn serve_ring(mut stream: isocast_ipc::Stream, offers: &Offers) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut token = [0; 16];
    stream.read_exact(&mut token)?;
    let surfaces = offers
        .lock()
        .unwrap()
        .remove(&Token(token))
        .context("unknown or consumed surface offer")?;
    stream.write_all(MAGIC)?;
    stream.write_all(&session()?.to_le_bytes())?;
    stream.write_all(&(surfaces.len() as u32).to_le_bytes())?;
    for surface in &surfaces {
        stream.write_all(&surface.0.name.0)?;
        for value in [
            surface.width(),
            surface.height(),
            surface.0.luid.0,
            surface.0.luid.1 as u32,
        ] {
            stream.write_all(&value.to_le_bytes())?;
        }
    }
    // Retain allocations while the client opens the names. A timeout merely
    // makes a late open fail; unique names cannot alias recycled handles.
    let mut ack = [0];
    stream.read_exact(&mut ack)?;
    ensure!(ack == [1], "invalid surface acknowledgment");
    Ok(())
}

pub fn fetch(service: &str, token: Token, timeout: Duration) -> Result<Vec<Surface>> {
    let mut stream = isocast_ipc::Stream::connect(service)?;
    stream.set_read_timeout(Some(timeout.min(Duration::from_secs(5))))?;
    stream.set_write_timeout(Some(timeout.min(Duration::from_secs(5))))?;
    stream.write_all(&token.0)?;
    let mut header = [0; 16];
    stream.read_exact(&mut header)?;
    ensure!(&header[..8] == MAGIC, "unknown shared surface protocol");
    let integer = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap());
    ensure!(
        integer(&header[8..12]) == session()?,
        "shared previews require the same Windows login session"
    );
    let count = integer(&header[12..]);
    ensure!(
        (2..=MAX_SLOTS as u32).contains(&count),
        "invalid shared ring count"
    );
    let mut surfaces = Vec::new();
    let mut device: Option<((u32, i32), ID3D12Device)> = None;
    for _ in 0..count {
        let mut record = [0; 32];
        stream.read_exact(&mut record)?;
        let token = Token(record[..16].try_into().unwrap());
        let (width, height) = (integer(&record[16..20]), integer(&record[20..24]));
        let luid = (integer(&record[24..28]), integer(&record[28..32]) as i32);
        let budget = Budget::new(width, height)?;
        if let Some((current, _)) = &device {
            ensure!(*current == luid, "mixed-adapter preview ring");
        }
        if device.is_none() {
            let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }?;
            let adapter: IDXGIAdapter1 = unsafe {
                factory.EnumAdapterByLuid(LUID {
                    LowPart: luid.0,
                    HighPart: luid.1,
                })
            }?;
            let mut raw = None;
            // D3D12 devices are process/adapter singletons; no extra command
            // queue or pixel staging is created just to open these handles.
            unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut raw) }?;
            device = Some((luid, raw.context("no shared surface adapter device")?));
        }
        let name = name(token);
        let handle = unsafe {
            device
                .as_ref()
                .unwrap()
                .1
                .OpenSharedHandleByName(PCWSTR(name.as_ptr()), GENERIC_ALL.0)
        }?;
        surfaces.push(Surface(Arc::new(Allocation {
            handle: Handle(unsafe { Owned::new(handle) }),
            name: token,
            width,
            height,
            luid,
            _budget: budget,
        })));
    }
    stream.write_all(&[1])?;
    Ok(surfaces)
}

#[cfg(feature = "wgpu")]
struct Retired {
    value: u64,
    completion: Option<Box<dyn FnOnce(bool) + Send>>,
}
#[cfg(feature = "wgpu")]
impl Retired {
    fn complete(&mut self, safe: bool) {
        if let Some(done) = self.completion.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| done(safe)));
        }
    }
}
#[cfg(feature = "wgpu")]
impl Drop for Retired {
    fn drop(&mut self) {
        self.complete(false);
    }
}

#[cfg(feature = "wgpu")]
pub fn check_render_queue(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
    let raw_device = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
        .context("preview device requires DX12")?
        .raw_device()
        .clone();
    let raw_queue = unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }
        .context("preview queue requires DX12")?
        .as_raw()
        .clone();
    let mut owner: Option<ID3D12Device> = None;
    unsafe { raw_queue.GetDevice(&mut owner) }?;
    ensure!(
        owner.as_ref() == Some(&raw_device),
        "preview rendering queue belongs to a different device"
    );
    Ok(())
}
#[cfg(feature = "wgpu")]
struct Retirement {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    next: u64,
    send: mpsc::SyncSender<Retired>,
}
#[cfg(feature = "wgpu")]
static RETIRE_FAILED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "wgpu")]
pub fn retirement_failed() -> bool {
    RETIRE_FAILED.load(Ordering::Acquire)
}

/// Called only after the caller has finished submitting every read of a frame.
/// Signal the actual rendering queue (including Slint/Skia's native submits),
/// not wgpu's presentation queue or only its last internal submission number.
#[cfg(feature = "wgpu")]
pub fn retire(queue: &wgpu::Queue, completion: Box<dyn FnOnce(bool) + Send>) -> Result<()> {
    static RETIREMENT: OnceLock<Mutex<Option<Retirement>>> = OnceLock::new();
    ensure!(
        !RETIRE_FAILED.load(Ordering::Acquire),
        "preview GPU retirement failed; restart the client"
    );
    let raw = unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }
        .context("preview retirement requires DX12")?
        .as_raw()
        .clone();
    let mut state = RETIREMENT.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some(current) = state.as_ref() {
        ensure!(
            current.queue == raw,
            "shared previews require one rendering queue per client"
        );
    }
    if state.is_none() {
        let mut device: Option<ID3D12Device> = None;
        unsafe { raw.GetDevice(&mut device) }?;
        let fence: ID3D12Fence = unsafe {
            device
                .context("no preview device")?
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
        }?;
        let event = Handle(unsafe { Owned::new(CreateEventW(None, false, false, None)?) });
        let (send, receive) = mpsc::sync_channel::<Retired>(256);
        let waiting = fence.clone();
        std::thread::Builder::new()
            .name("preview GPU retirement".into())
            .spawn(move || {
                while let Ok(mut job) = receive.recv() {
                    let value = job.value;
                    let result = (|| -> Result<()> {
                        unsafe { waiting.SetEventOnCompletion(value, *event.0) }?;
                        ensure!(
                            unsafe { WaitForSingleObject(*event.0, 5000) } == WAIT_OBJECT_0,
                            "preview GPU completion timed out"
                        );
                        let done = unsafe { waiting.GetCompletedValue() };
                        ensure!(done != u64::MAX && done >= value, "preview GPU removed");
                        Ok(())
                    })();
                    if let Err(error) = result {
                        RETIRE_FAILED.store(true, Ordering::Release);
                        eprintln!("{error:#}; viewer slots will not be reused");
                        // An outstanding native event registration must not target
                        // a closed/recycled HANDLE. One bounded quarantine/client.
                        std::mem::forget((waiting, event));
                        return;
                    }
                    job.complete(true);
                }
            })?;
        *state = Some(Retirement {
            queue: raw,
            fence,
            next: 1,
            send,
        });
    }
    let state = state.as_mut().unwrap();
    let value = state.next;
    state.next = value.checked_add(1).context("preview fence exhausted")?;
    let job = Retired {
        value,
        completion: Some(completion),
    };
    unsafe { state.queue.Signal(&state.fence, value) }?;
    state
        .send
        .try_send(job)
        .map_err(|_| anyhow::anyhow!("preview retirement queue full/failed; slot not released"))?;
    Ok(())
}
