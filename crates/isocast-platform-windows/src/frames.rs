use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter1,
    IDXGIFactory4, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use crate::native::{Event, SharedHandle};

pub const MAX_POOL_FRAMES: usize = 32;

/// GPU storage; ten-bit storage does not imply HDR colorimetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// DXGI_FORMAT_B8G8R8A8_UNORM.
    Bgra8,
    /// DXGI_FORMAT_R10G10B10A2_UNORM.
    Rgba10,
    /// DXGI_FORMAT_NV12, qualified SDR Rec.709 limited-range input, progressive
    /// or woven from two interleaved fields without changing their rows,
    /// with left-sited chroma. The producer must validate the color metadata;
    /// copying a texture does not establish it. Read-only, never an output.
    Nv12,
}

/// A D3D11 device with a GPU timeline shared by its captures and uploads.
/// Capture, compositor and encoders use the same DX12-compatible adapter;
/// there is no cross-adapter copy or readback fallback.
#[derive(Clone)]
pub struct GraphicsDevice(Arc<Device>);

struct Device {
    raw: ID3D11Device5,
    submit: Mutex<Submit>,
    fence: ID3D11Fence,
    shared_fence: SharedHandle,
    name: String,
    luid: (u32, i32),
}

struct Submit {
    context: ID3D11DeviceContext4,
    next: u64,
}

impl GraphicsDevice {
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = isocast_surfaces::dx12_adapter(&instance, None)?;
        Self::on_adapter(isocast_surfaces::dx12_adapter_luid(&adapter)?)
    }

    /// Open the exact adapter selected by the parent, never reselect by index
    /// or power preference in the capture helper process.
    pub(crate) fn on_adapter(luid: (u32, i32)) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(LUID {
                    LowPart: luid.0,
                    HighPart: luid.1,
                })
                .with_context(|| format!("capture adapter {luid:?} is unavailable"))?;
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(desc.Description.len())],
            );
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("could not open the D3D11 capture device")?;
            let raw: ID3D11Device5 = device.context("D3D11 returned no device")?.cast()?;
            let context: ID3D11DeviceContext4 =
                context.context("D3D11 returned no context")?.cast()?;
            // WGC also submits on this device from its internal worker. Our
            // mutex alone cannot serialize those immediate-context calls.
            let multithread: ID3D11Multithread = context.cast()?;
            let _ = multithread.SetMultithreadProtected(true);
            let mut fence = None;
            raw.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)?;
            let fence: ID3D11Fence = fence.context("D3D11 returned no fence")?;
            let shared_fence = SharedHandle::new(fence.CreateSharedHandle(
                None,
                GENERIC_ALL.0,
                PCWSTR::null(),
            )?)?;
            Ok(Self(Arc::new(Device {
                raw,
                submit: Mutex::new(Submit { context, next: 1 }),
                fence,
                shared_fence,
                name,
                luid: (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart),
            })))
        }
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn adapter_luid(&self) -> (u32, i32) {
        self.0.luid
    }

    pub(crate) fn raw(&self) -> &ID3D11Device5 {
        &self.0.raw
    }

    pub(crate) fn owns(&self, frame: &Frame) -> bool {
        Arc::ptr_eq(&self.0, &frame.device.0)
    }

    pub(crate) fn completed(&self) -> Result<u64> {
        let value = unsafe { self.0.fence.GetCompletedValue() };
        if value == u64::MAX {
            bail!("D3D11 capture device was removed");
        }
        Ok(value)
    }

    pub(crate) fn notify(&self, value: u64, event: &Event) -> Result<()> {
        unsafe { self.0.fence.SetEventOnCompletion(value, event.handle()) }?;
        Ok(())
    }

    /// Import a read-only broker picture without another GPU copy. The native
    /// texture may be cached, but EVERY picture gets its own slot and remote
    /// lease. Consumers retain that slot through their GPU completion as usual.
    pub(crate) fn import_shared(
        &self,
        texture: ID3D11Texture2D,
        shared: Arc<SharedHandle>,
        fence: &ID3D11Fence,
        ready: u64,
        lease: Box<dyn Send + Sync>,
    ) -> Result<Frame> {
        let owner: ID3D11Device5 = unsafe { texture.GetDevice() }?.cast()?;
        let fence_owner: ID3D11Device5 = unsafe { fence.GetDevice() }?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if owner != *self.raw()
            || fence_owner != *self.raw()
            || !(1..=16384).contains(&desc.Width)
            || !(1..=16384).contains(&desc.Height)
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || desc.MipLevels != 1
            || desc.ArraySize != 1
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
            || desc.Usage != D3D11_USAGE_DEFAULT
            || desc.CPUAccessFlags != 0
            || desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0
            || desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32 == 0
        {
            bail!(
                "capture helper texture must be same-device GPU-only BGRA8, single mip/slice/sample, with a shader-readable NT sharing handle"
            );
        }
        let completed = unsafe { fence.GetCompletedValue() };
        if ready == 0 || completed == u64::MAX || completed < ready {
            // In particular NEVER enqueue a wait on an uncompleted child
            // fence: death of that child must not strand our GPU queue.
            bail!("capture helper published an unfinished copy or lost its GPU device");
        }
        let mut submit = self.0.submit.lock();
        let value = submit.next;
        submit.next = value
            .checked_add(1)
            .context("D3D11 import fence exhausted")?;
        unsafe {
            submit.context.Wait(fence, ready)?;
            submit.context.Signal(&self.0.fence, value)?;
            submit.context.Flush();
            self.raw().GetDeviceRemovedReason()?;
        }
        // This local signal only imports producer readiness. It does NOT
        // acknowledge the picture: the slot's lease must survive all later
        // GpuCompletion/uncached GPU imports and NVENC pending-frame owners.
        Ok(Frame {
            slot: Arc::new(Slot {
                texture,
                shared,
                width: desc.Width,
                height: desc.Height,
                format: PixelFormat::Bgra8,
                output: false,
                ready: AtomicU64::new(value),
                _lease: Some(lease),
            }),
            device: self.clone(),
        })
    }
}

/// One lease on an owned GPU texture. Capture pictures are immutable; output
/// pictures may be drawn into before publication. All clones and uncached GPU
/// imports keep the slot out of its pool. Cached GPU users must retain a frame
/// through completion. Never write an output while a consumer is using it.
/// Capture slots additionally wait for their GPU copy before being recycled.
///
/// Consumers wait on the D3D11 fence for capture copies and CPU uploads. A
/// GPU-written output must also await its producer's `GpuCompletion` before
/// native consumption: the D3D11 fence does not track wgpu writes.
#[derive(Clone)]
pub struct Frame {
    slot: Arc<Slot>,
    device: GraphicsDevice,
}

struct Slot {
    texture: ID3D11Texture2D,
    shared: Arc<SharedHandle>,
    width: u32,
    height: u32,
    format: PixelFormat,
    output: bool,
    ready: AtomicU64,
    // Last field: release the remote immutable picture after its local native
    // references. Cached GPU imports must not cache this per-picture lease.
    _lease: Option<Box<dyn Send + Sync>>,
}

impl Slot {
    fn new(
        device: &GraphicsDevice,
        width: u32,
        height: u32,
        format: PixelFormat,
        output: bool,
    ) -> Result<Arc<Self>> {
        if format == PixelFormat::Nv12
            && (output
                || width == 0
                || height == 0
                || !width.is_multiple_of(2)
                || !height.is_multiple_of(2))
        {
            bail!("NV12 frames require read-only input with positive even dimensions");
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: match format {
                PixelFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
                PixelFormat::Rgba10 => DXGI_FORMAT_R10G10B10A2_UNORM,
                PixelFormat::Nv12 => DXGI_FORMAT_NV12,
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: if format == PixelFormat::Nv12 {
                D3D11_BIND_SHADER_RESOURCE.0 as u32
            } else {
                (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32
            },
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED).0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe {
            device
                .raw()
                .CreateTexture2D(&desc, None, Some(&mut texture))
        }?;
        let texture = texture.context("D3D11 returned no texture")?;
        let resource: IDXGIResource1 = texture.cast()?;
        let shared = unsafe {
            SharedHandle::new(resource.CreateSharedHandle(
                None,
                (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                PCWSTR::null(),
            )?)?
        };
        Ok(Arc::new(Self {
            texture,
            shared: Arc::new(shared),
            width,
            height,
            format,
            output,
            ready: AtomicU64::new(0),
            _lease: None,
        }))
    }
}

impl Frame {
    pub fn width(&self) -> u32 {
        self.slot.width
    }
    pub fn height(&self) -> u32 {
        self.slot.height
    }
    pub fn pixel_format(&self) -> PixelFormat {
        self.slot.format
    }
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.slot.texture
    }
    /// D3D11 capture/upload timeline only, not completion of wgpu writes.
    pub fn fence(&self) -> &ID3D11Fence {
        &self.device.0.fence
    }
    /// The last D3D11 capture/upload signal. GPU output consumers must also
    /// wait for the producer's `GpuCompletion` before using the picture.
    pub fn ready_value(&self) -> u64 {
        self.slot.ready.load(Ordering::Acquire)
    }
    pub fn adapter_luid(&self) -> (u32, i32) {
        self.device.adapter_luid()
    }

    pub(crate) fn shared_texture(&self) -> HANDLE {
        self.slot.shared.handle()
    }

    pub(crate) fn shared_fence(&self) -> HANDLE {
        self.device.0.shared_fence.handle()
    }

    pub(crate) fn new_output(device: GraphicsDevice, width: u32, height: u32) -> Result<Self> {
        let slot = Slot::new(&device, width, height, PixelFormat::Bgra8, true)?;
        Ok(Self { slot, device })
    }

    pub(crate) fn is_output(&self) -> bool {
        self.slot.output
    }

    /// Only the output pool's own lease remains. Unlike capture recycling,
    /// this need not wait for an earlier D3D11 upload: another upload is
    /// ordered on the same context, and a GPU target waits on `ready_value`.
    pub(crate) fn is_idle_output(&self) -> bool {
        self.is_output() && Arc::strong_count(&self.slot) == 1
    }

    /// Upload CPU-generated BGRA pixels to an idle output. On failure the
    /// caller must retire its whole pool, since Signal may not have reached
    /// the queue. This method never runs the user's drawing callback.
    pub(crate) fn upload(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.is_output() {
            bail!("captured Windows frames are read-only");
        }
        let slot = &self.slot;
        let stride = slot
            .width
            .checked_mul(4)
            .context("output row size overflow")?;
        let len = (stride as usize)
            .checked_mul(slot.height as usize)
            .context("output byte size overflow")?;
        if bytes.len() != len {
            bail!("output upload requires {len} tightly packed BGRA bytes");
        }
        self.render(|context, target| {
            unsafe { context.UpdateSubresource(target, 0, None, bytes.as_ptr().cast(), stride, 0) };
            Ok(())
        })
    }

    /// One native GPU write transaction into an exclusively owned output.
    /// The device lock also excludes other immediate-context users while
    /// shader state is bound. A failed call poisons the caller's output pool.
    pub(crate) fn render(
        &mut self,
        draw: impl FnOnce(&ID3D11DeviceContext4, &ID3D11Texture2D) -> Result<()>,
    ) -> Result<()> {
        if !self.is_output() {
            bail!("captured Windows frames are read-only");
        }
        let slot = Arc::get_mut(&mut self.slot).context("output frame is still in use")?;
        let mut submit = self.device.0.submit.lock();
        let multithread: ID3D11Multithread = submit.context.cast()?;
        unsafe { multithread.Enter() };
        struct Locked(ID3D11Multithread);
        impl Drop for Locked {
            fn drop(&mut self) {
                unsafe { self.0.Leave() };
            }
        }
        let _locked = Locked(multithread);
        let value = submit.next;
        submit.next = value
            .checked_add(1)
            .context("D3D11 output fence exhausted")?;
        slot.ready.store(value, Ordering::Release);
        draw(&submit.context, &slot.texture)?;
        unsafe {
            submit
                .context
                .Signal(&self.device.0.fence, value)
                .context("could not signal the D3D11 output upload")?;
            submit.context.Flush();
            self.device
                .raw()
                .GetDeviceRemovedReason()
                .context("D3D11 output upload device was removed")?;
        }
        Ok(())
    }

    /// For explicit CPU-side consumers/diagnostics only. The compositor and
    /// GPU consumers use fence waits; NVENC's D3D11 input mapping synchronizes
    /// its private GPU copy instead of using this diagnostic polling loop.
    /// This does not wait for wgpu writes to an output frame.
    pub fn wait_ready(&self, timeout: Duration) -> Result<bool> {
        let started = Instant::now();
        loop {
            if self.device.completed()? >= self.ready_value() {
                return Ok(true);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(false);
            }
            // No event registration that could outlive a timed-out caller.
            // Native GPU consumers use fence(), never this diagnostic poll.
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
        }
    }
}

/// Bounded owned GPU frames, shared across sizes and pixel formats.
pub struct TexturePool {
    device: GraphicsDevice,
    slots: Vec<Arc<Slot>>,
}

impl TexturePool {
    pub fn new(device: GraphicsDevice) -> Self {
        Self {
            device,
            slots: Vec::new(),
        }
    }

    /// Preserve CPU-delivered NV12 with one upload, without RGB conversion.
    /// Rows are top-down; UV starts at pitch*height, with the same pitch.
    pub(crate) fn upload_nv12(
        &mut self,
        width: u32,
        height: u32,
        bytes: &[u8],
        pitch: u32,
    ) -> Result<Option<Frame>> {
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || pitch < width
            || u64::from(pitch) * u64::from(height + height / 2) > bytes.len() as u64
        {
            bail!("camera.frame: invalid NV12 dimensions, pitch or byte length");
        }
        let completed = self.device.completed()?;
        self.slots.retain(|slot| {
            (slot.width == width && slot.height == height && slot.format == PixelFormat::Nv12)
                || Arc::strong_count(slot) > 1
                || slot.ready.load(Ordering::Acquire) > completed
        });
        let slot = self.slots.iter().find(|slot| {
            slot.width == width
                && slot.height == height
                && slot.format == PixelFormat::Nv12
                && Arc::strong_count(slot) == 1
                && slot.ready.load(Ordering::Acquire) <= completed
        });
        let slot = match slot {
            Some(slot) => slot.clone(),
            None if self.slots.len() == MAX_POOL_FRAMES => return Ok(None),
            None => {
                let slot = Slot::new(&self.device, width, height, PixelFormat::Nv12, false)?;
                self.slots.push(slot.clone());
                slot
            }
        };
        let mut submit = self.device.0.submit.lock();
        let value = submit.next;
        submit.next = value
            .checked_add(1)
            .context("D3D11 camera upload fence exhausted")?;
        slot.ready.store(value, Ordering::Release);
        unsafe {
            submit.context.UpdateSubresource(
                &slot.texture,
                0,
                None,
                bytes.as_ptr().cast(),
                pitch,
                0,
            );
            submit.context.Signal(&self.device.0.fence, value)?;
            submit.context.Flush();
            self.device.raw().GetDeviceRemovedReason()?;
        }
        Ok(Some(Frame {
            slot,
            device: self.device.clone(),
        }))
    }

    /// Copy the top-left `width` by `height` region of subresource zero without
    /// CPU readback. See `copy_subresource` for source and lifetime requirements.
    pub fn copy(
        &mut self,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<Option<Frame>> {
        self.copy_subresource(source, 0, width, height)
    }

    /// Copy the top-left `width` by `height` region of one array slice into an
    /// owned, single-layer texture without CPU readback or a color conversion.
    /// Returns `None` when all `MAX_POOL_FRAMES` slots are still in use.
    /// Supports BGRA8, RGB10A2 and NV12 default-usage, single-mip/sample textures;
    /// `subresource` must name an existing array slice. NV12 requires positive
    /// even dimensions, including the visible crop, and the qualified Rec.709
    /// limited-range color contract documented on `PixelFormat::Nv12`.
    ///
    /// The source must belong to this pool's device. External producers must
    /// finish submitting writes on the shared device's immediate context
    /// before this call; cross-API producers must synchronize their writes
    /// with that device first. Do not overwrite or recycle the source until
    /// the returned frame's fence reaches its ready value (or order subsequent
    /// writes after this copy on the same immediate context). For MF cameras,
    /// retain the source sample as well as the returned frame until completion.
    pub fn copy_subresource(
        &mut self,
        source: &ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
    ) -> Result<Option<Frame>> {
        self.copy_with(source, subresource, width, height, |_, _| Ok(()))
    }

    /// Finish a capture write BEFORE publishing its Frame and signaling ready.
    /// The destination is reserved here, never supplied by a caller: only an
    /// idle slot owned exclusively by this pool is writable. Consumer-held or
    /// foreign Frames cannot enter this path. On error retire the whole pool
    /// and quarantine the source, since GPU work may precede a failed Signal.
    pub(crate) fn copy_with(
        &mut self,
        source: &ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
        finish: impl FnOnce(&ID3D11DeviceContext4, &ID3D11Texture2D) -> Result<()>,
    ) -> Result<Option<Frame>> {
        let owner: ID3D11Device5 = unsafe { source.GetDevice() }?.cast()?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        let format = match source_desc.Format {
            DXGI_FORMAT_B8G8R8A8_UNORM => PixelFormat::Bgra8,
            DXGI_FORMAT_R10G10B10A2_UNORM => PixelFormat::Rgba10,
            DXGI_FORMAT_NV12 => PixelFormat::Nv12,
            other => bail!(
                "copy source format {other:?}: valid formats are B8G8R8A8_UNORM, R10G10B10A2_UNORM and NV12"
            ),
        };
        if owner != *self.device.raw()
            || width == 0
            || height == 0
            || width > source_desc.Width
            || height > source_desc.Height
            || source_desc.MipLevels != 1
            || subresource >= source_desc.ArraySize
            || source_desc.SampleDesc.Count != 1
            || source_desc.SampleDesc.Quality != 0
            || source_desc.Usage != D3D11_USAGE_DEFAULT
            || source_desc.CPUAccessFlags != 0
        {
            bail!(
                "copy source must be same-device, GPU-only default usage, single mip/sample with a valid array subresource and nonzero copy dimensions within the texture"
            );
        }
        if format == PixelFormat::Nv12
            && [width, height, source_desc.Width, source_desc.Height]
                .iter()
                .any(|size| !size.is_multiple_of(2))
        {
            bail!("NV12 copy source and visible dimensions must be even");
        }
        let completed = self.device.completed()?;
        // Discard only idle old-size/format textures; retained pictures keep
        // their layout and still count toward the same 32-frame bound.
        self.slots.retain(|slot| {
            (slot.width == width && slot.height == height && slot.format == format)
                || Arc::strong_count(slot) > 1
                || slot.ready.load(Ordering::Acquire) > completed
        });
        let slot = self.slots.iter().find(|slot| {
            slot.width == width
                && slot.height == height
                && slot.format == format
                && Arc::strong_count(slot) == 1
                && slot.ready.load(Ordering::Acquire) <= completed
        });
        let slot = match slot {
            Some(slot) => slot.clone(),
            None if self.slots.len() == MAX_POOL_FRAMES => return Ok(None),
            None => {
                let slot = Slot::new(&self.device, width, height, format, false)?;
                self.slots.push(slot.clone());
                slot
            }
        };
        let mut submit = self.device.0.submit.lock();
        let value = submit.next;
        submit.next = value
            .checked_add(1)
            .context("D3D11 capture fence exhausted")?;
        // Mark busy BEFORE submission; a failed Signal must never allow this
        // slot to be reused while its copy might still be executing.
        slot.ready.store(value, Ordering::Release);
        unsafe {
            let bounds = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            };
            submit.context.CopySubresourceRegion(
                &slot.texture,
                0,
                0,
                0,
                0,
                source,
                subresource,
                Some(&bounds),
            );
            finish(&submit.context, &slot.texture)?;
            submit.context.Signal(&self.device.0.fence, value)?;
            submit.context.Flush();
        }
        Ok(Some(Frame {
            slot,
            device: self.device.clone(),
        }))
    }
}
