//! D3D11On12 draws on Slint's existing DX12 device and queue. EndDraw stages
//! the pixels for the parent's Commit, together with the box and meter.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use isocast_client::shared::SharedFrame;
use slint::wgpu_30::wgpu;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Direct3D11on12::{
    D3D11_RESOURCE_FLAGS, D3D11On12CreateDevice, ID3D11On12Device,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_RESOURCE_STATE_COMMON, ID3D12Device,
};
use windows::Win32::Graphics::DirectComposition::{
    IDCompositionDevice, IDCompositionDevice2, IDCompositionSurface, IDCompositionSurfaceFactory,
    IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface, IDXGISurface2};
use windows::core::{IUnknown, Interface};

pub struct Drawing {
    gpu: crate::gpu::Gpu,
    device: ID3D12Device,
    device11: ID3D11Device,
    context: ID3D11DeviceContext,
    on12: ID3D11On12Device,
    factory: IDCompositionSurfaceFactory,
    failed: Cell<bool>,
    _composition: IDCompositionDevice,
}

impl Drawing {
    pub fn new(gpu: &crate::gpu::Gpu, composition: &IDCompositionDevice) -> Result<Rc<Self>> {
        isocast_client::shared::register_gpu(&gpu.device, &gpu.queue)?;
        let device = unsafe { gpu.device.as_hal::<wgpu::hal::api::Dx12>() }
            .context("native preview requires DX12")?
            .raw_device()
            .clone();
        let queue = unsafe { gpu.queue.as_hal::<wgpu::hal::api::Dx12>() }
            .context("native preview requires a DX12 queue")?
            .as_raw()
            .clone();
        ensure!(
            unsafe { queue.GetDesc() }.Type == D3D12_COMMAND_LIST_TYPE_DIRECT,
            "native preview requires the rendering direct queue"
        );
        let mut device11 = None;
        let mut context = None;
        unsafe {
            D3D11On12CreateDevice(
                &device,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT.0,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                Some(&[Some(queue.cast()?)]),
                0,
                Some(&mut device11),
                Some(&mut context),
                None,
            )
        }
        .context("create preview On12 wrapper on the rendering queue")?;
        let device11: ID3D11Device = device11.context("On12 returned no device")?;
        let context = context.context("On12 returned no immediate context")?;
        let on12 = device11.cast()?;
        let dxgi: IDXGIDevice = device11.cast()?;
        let composition2: IDCompositionDevice2 = composition.cast()?;
        let factory = unsafe { composition2.CreateSurfaceFactory(&dxgi) }
            .context("create transactional preview surface factory")?;
        Ok(Rc::new(Self {
            gpu: gpu.clone(),
            device,
            device11,
            context,
            on12,
            factory,
            failed: Cell::new(false),
            _composition: composition.clone(),
        }))
    }

    fn check(&self) -> Result<()> {
        ensure!(!self.failed.get(), "native preview drawing failed");
        unsafe { self.device.GetDeviceRemovedReason() }.context("preview DX12 device removed")?;
        unsafe { self.device11.GetDeviceRemovedReason() }.context("preview On12 device removed")
    }
}

struct Wrapped {
    resource: ID3D11Resource,
    // Wrapped resources are the exception to On12's deferred destruction rule:
    // keep the original DX12 allocation alive until its queued reads finish.
    _texture: wgpu::Texture,
}

pub struct Content {
    visual: IDCompositionVisual,
    drawing: Rc<Drawing>,
    surface: Option<IDCompositionSurface>,
    size: (u32, u32),
    attached: bool,
    images: HashMap<(u64, u32), Wrapped>,
    retired: Option<Arc<AtomicBool>>,
}

impl Content {
    pub fn new(
        gpu: &crate::gpu::Gpu,
        visual: &IDCompositionVisual,
        drawing: Rc<Drawing>,
    ) -> Result<Self> {
        drawing.check()?;
        ensure!(
            drawing.gpu.device == gpu.device && drawing.gpu.queue == gpu.queue,
            "preview drawing belongs to a different device/queue"
        );
        Ok(Self {
            visual: visual.clone(),
            drawing,
            surface: None,
            size: (0, 0),
            attached: false,
            images: HashMap::new(),
            retired: None,
        })
    }

    /// One GPU copy, staged for the parent's Commit. The owner skips hidden
    /// windows and retains frame until return; its Release fences the SAME queue
    /// after our Flush before the producer can reuse the source slot.
    pub fn show(&mut self, frame: &SharedFrame) -> Result<bool> {
        let result = self.show_frame(frame);
        self.checked(result)
    }

    fn show_frame(&mut self, frame: &SharedFrame) -> Result<bool> {
        self.drawing.check()?;
        let key = frame.cache_key();
        if let Some(done) = &self.retired {
            // Native video can run without any Slint/wgpu redraws. Service the
            // retirement callback ourselves until it finishes, without waiting.
            self.drawing.gpu.device.poll(wgpu::PollType::Poll)?;
            if done.load(Ordering::Acquire) {
                self.retired = None;
            }
        }
        if self.images.keys().any(|key0| key0.0 != key.0) {
            // Bound retention to the current ring and one retiring ring.
            if self.retired.is_some() {
                return Ok(false);
            }
            self.retire_images();
        }
        self.resize(frame.width(), frame.height())?;
        let fresh = !self.images.contains_key(&key);
        if fresh {
            let texture = frame.texture(&self.drawing.gpu.device)?;
            let raw = unsafe { texture.as_hal::<wgpu::hal::api::Dx12>() }
                .context("shared preview texture is not DX12")?;
            let mut resource: Option<ID3D11Resource> = None;
            unsafe {
                self.drawing.on12.CreateWrappedResource(
                    raw.raw_resource(),
                    &D3D11_RESOURCE_FLAGS::default(),
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_COMMON,
                    &mut resource,
                )
            }
            .context("wrap shared preview source")?;
            drop(raw);
            self.images.insert(
                key,
                Wrapped {
                    resource: resource.context("On12 returned no wrapped source")?,
                    _texture: texture,
                },
            );
        }
        let drawing = self.drawing.clone();
        let resource = self.images[&key].resource.clone();
        // CreateWrappedResource starts acquired. Only cached, previously
        // released resources need AcquireWrappedResources on subsequent frames.
        if !fresh {
            unsafe {
                drawing
                    .on12
                    .AcquireWrappedResources(&[Some(resource.clone())])
            };
        }
        let source = Acquired {
            drawing: &drawing,
            resource,
        };
        let mut draw = Draw::begin(&drawing, self.surface.as_ref().unwrap(), Some(source))?;
        let (target, subresource, bounds) = draw.target(self.size)?;
        unsafe {
            drawing.context.CopySubresourceRegion(
                &target,
                subresource,
                bounds.left,
                bounds.top,
                0,
                &draw.source.as_ref().unwrap().resource,
                0,
                None,
            );
        }
        draw.finish()?;
        self.attach()?;
        Ok(true)
    }

    /// Static, tightly packed premultiplied RGBA palette only; no video readback.
    pub fn upload(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        let result = (|| {
            self.drawing.check()?;
            self.resize(width, height)?;
            ensure!(
                rgba.len() == width as usize * height as usize * 4,
                "palette RGBA length does not match {width}x{height}"
            );
            let drawing = self.drawing.clone();
            let mut draw = Draw::begin(&drawing, self.surface.as_ref().unwrap(), None)?;
            let (target, subresource, bounds) = draw.target(self.size)?;
            unsafe {
                drawing.context.UpdateSubresource(
                    &target,
                    subresource,
                    Some(&bounds),
                    rgba.as_ptr().cast(),
                    width * 4,
                    0,
                )
            };
            draw.finish()?;
            self.attach()
        })();
        self.checked(result)
    }

    pub fn clear(&mut self) -> Result<()> {
        unsafe { self.visual.SetContent(None::<&IUnknown>) }.context("detach native preview")?;
        self.attached = false;
        Ok(())
    }

    pub fn content(&self) -> Option<&IDCompositionSurface> {
        self.surface.as_ref()
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "native preview dimensions must be 1..=8192"
        );
        if self.size != (width, height) {
            self.surface = Some(
                unsafe {
                    self.drawing.factory.CreateSurface(
                        width,
                        height,
                        DXGI_FORMAT_R8G8B8A8_UNORM,
                        DXGI_ALPHA_MODE_PREMULTIPLIED,
                    )
                }
                .context("create transactional preview surface")?,
            );
            self.size = (width, height);
            self.attached = false;
        }
        Ok(())
    }

    fn attach(&mut self) -> Result<()> {
        if !self.attached {
            unsafe { self.visual.SetContent(self.content().unwrap()) }
                .context("attach native preview")?;
            self.attached = true;
        }
        Ok(())
    }

    fn checked<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.drawing.failed.set(true);
            let _ = self.clear();
        }
        result
    }

    fn retire_images(&mut self) {
        if self.images.is_empty() {
            return;
        }
        let images = std::mem::take(&mut self.images);
        let device = self.drawing.device.clone();
        if unsafe { device.GetDeviceRemovedReason() }.is_err() {
            // Device loss isn't proof that externally submitted reads finished.
            std::mem::forget(images);
            return;
        }
        let done = Arc::new(AtomicBool::new(false));
        self.retired = Some(done.clone());
        // Every source has been released and flushed. An empty wgpu submission
        // fences preceding native reads too; on_submitted_work_done alone would
        // only cover wgpu's old submission number. No native fence or CPU wait.
        self.drawing.gpu.queue.submit([]);
        self.drawing.gpu.queue.on_submitted_work_done(move || {
            if unsafe { device.GetDeviceRemovedReason() }.is_err() {
                std::mem::forget(images);
            } else {
                drop(images);
            }
            done.store(true, Ordering::Release);
        });
    }
}

impl Drop for Content {
    fn drop(&mut self) {
        let _ = self.clear();
        self.retire_images();
    }
}

struct Acquired<'a> {
    drawing: &'a Drawing,
    resource: ID3D11Resource,
}
impl Drop for Acquired<'_> {
    fn drop(&mut self) {
        unsafe {
            self.drawing
                .on12
                .ReleaseWrappedResources(&[Some(self.resource.clone())]);
            self.drawing.context.Flush();
        }
    }
}

struct Draw<'a> {
    drawing: &'a Drawing,
    surface: IDCompositionSurface,
    update: IDXGISurface,
    offset: POINT,
    source: Option<Acquired<'a>>,
    ended: bool,
}

impl<'a> Draw<'a> {
    fn begin(
        drawing: &'a Drawing,
        surface: &IDCompositionSurface,
        source: Option<Acquired<'a>>,
    ) -> Result<Self> {
        let mut offset = POINT::default();
        let update =
            unsafe { surface.BeginDraw(None, &mut offset) }.context("begin preview draw")?;
        Ok(Self {
            drawing,
            surface: surface.clone(),
            update,
            offset,
            source,
            ended: false,
        })
    }

    fn target(&self, (width, height): (u32, u32)) -> Result<(ID3D11Texture2D, u32, D3D11_BOX)> {
        let update: IDXGISurface2 = self.update.cast()?;
        let mut subresource = 0;
        let texture: ID3D11Texture2D = unsafe { update.GetResource(&mut subresource) }?;
        ensure!(
            unsafe { texture.GetDevice() }? == self.drawing.device11,
            "preview draw texture belongs to a different device"
        );
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        ensure!(
            desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM
                && desc.MipLevels == 1
                && subresource < desc.ArraySize
                && desc.SampleDesc.Count == 1
                && self.offset.x >= 0
                && self.offset.y >= 0
                && self.offset.x as u64 + u64::from(width) <= u64::from(desc.Width)
                && self.offset.y as u64 + u64::from(height) <= u64::from(desc.Height),
            "preview draw rectangle/format does not match its texture"
        );
        Ok((
            texture,
            subresource,
            D3D11_BOX {
                left: self.offset.x as u32,
                top: self.offset.y as u32,
                front: 0,
                right: self.offset.x as u32 + width,
                bottom: self.offset.y as u32 + height,
                back: 1,
            },
        ))
    }

    fn finish(&mut self) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        self.ended = true; // Never retry an EndDraw whose ownership is uncertain.
        if let Some(source) = self.source.take() {
            drop(source);
        } else {
            unsafe { self.drawing.context.Flush() };
        }
        let result = unsafe { self.surface.EndDraw() }.context("end preview draw");
        // Flush any work EndDraw added before returning to Slint/raw retirement
        // on the same queue. There is no application fence dependency or wait.
        unsafe { self.drawing.context.Flush() };
        let result = result.and_then(|()| self.drawing.check());
        if result.is_err() {
            self.drawing.failed.set(true);
        }
        result
    }
}

impl Drop for Draw<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("preview draw cleanup: {error:#}");
        }
    }
}
