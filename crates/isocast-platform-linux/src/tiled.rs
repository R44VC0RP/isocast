//! Import single-plane, implicitly synchronized DRM-modifier images on the
//! *engine* Vulkan device. Only the staging buffer is visible to wgpu's
//! tracker; native commands acquire and release the foreign image in GENERAL.
use std::ffi::c_void;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::pipewire_video::timed;
use anyhow::{Context, Result, ensure};
use isocast_engine::gpu::Gpu;
use pipewire::spa::{param::video::VideoFormat, pod::Pod};
use wgpu::hal::Device as _;
use wgpu::hal::api::Vulkan;

/// SPA_FORMAT_VIDEO_deviceId (newer than the minimum PipeWire headers).
pub(super) const DEVICE_ID: u32 = 0x20016;
const GPU_WAIT: Duration = Duration::from_secs(5);
static FAULTED: AtomicBool = AtomicBool::new(false);

pub(super) fn disable_after_fault() {
    FAULTED.store(true, Ordering::Release);
}

fn enabled() -> bool {
    !FAULTED.load(Ordering::Acquire)
        && std::env::var_os("ISOCAST_DISABLE_TILED_DMABUF").as_deref()
            != Some(std::ffi::OsStr::new("1"))
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Modifier {
    pub value: u64,
    pub width: u32,
    pub height: u32,
}

unsafe extern "C" {
    fn iso_tiled_available() -> i32;
    fn iso_tiled_render_node(
        phys: *const c_void,
        get: *const c_void,
        major: *mut u32,
        minor: *mut u32,
    ) -> i32;
    fn iso_tiled_modifiers(
        phys: *const c_void,
        formats: *const c_void,
        images: *const c_void,
        bgra: i32,
        out: *mut Modifier,
        cap: u32,
    ) -> u32;
    fn iso_tiled_copy(
        cmd: *const c_void,
        barriers: *const c_void,
        copy: *const c_void,
        family: u32,
        image: *const c_void,
        buffer: *const c_void,
        w: u32,
        h: u32,
    );
    fn iso_tiled_transfer_to_host(cmd: *const c_void, barriers: *const c_void);
}

pub(super) fn open_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Option<(wgpu::Device, wgpu::Queue)> {
    let feature = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
    // SAFETY: compile-time header check, no arguments.
    if !enabled() || unsafe { iso_tiled_available() } == 0 || !adapter.features().contains(feature)
    {
        return None;
    }
    let mut desc = desc.clone();
    desc.required_features |= feature;
    let opened = {
        // SAFETY: no ownership of the adapter is taken. Only add advertised
        // extensions; the underlying device is adopted by this same adapter.
        let hal = unsafe { adapter.as_hal::<Vulkan>() }?;
        let extensions = [
            c"VK_EXT_queue_family_foreign",
            c"VK_EXT_physical_device_drm",
        ];
        if !extensions
            .iter()
            .all(|ext| hal.physical_device_capabilities().supports_extension(ext))
        {
            return None;
        }
        // SAFETY: only append extensions whose availability was checked on
        // this adapter; preserve the feature chain, queue setup and limits.
        unsafe {
            hal.open_with_callback(
                desc.required_features,
                &desc.required_limits,
                &desc.memory_hints,
                Some(Box::new(move |args| {
                    for ext in extensions {
                        if !args.extensions.contains(&ext) {
                            args.extensions.push(ext);
                        }
                    }
                })),
            )
        }
        .ok()?
    };
    // SAFETY: `opened` came from this adapter with exactly desc's features
    // and limits. Ownership of that device and queue is transferred once.
    unsafe { adapter.create_device_from_hal::<Vulkan>(opened, &desc) }.ok()
}

pub(super) fn device_id(pod: &Pod) -> Result<Option<u64>> {
    use pipewire::spa::{sys, utils::Id};
    let Some(prop) = pod.as_object()?.find_prop(Id(DEVICE_ID)) else {
        return Ok(None);
    };
    let mut value = prop.value();
    // EnumFormat can carry bare Bytes, but spa_pod_filter wraps the same
    // value in Choice(None, Bytes) on the negotiated Format. Never use the
    // default of an unfixed choice as proof of the exporting device.
    if value.is_choice() {
        let (mut count, mut kind) = (0, 0);
        // SAFETY: is_choice checked the choice-body size. get_values writes
        // the number of complete elements and points inside the borrowed pod.
        let child = unsafe { sys::spa_pod_get_values(value.as_raw_ptr(), &mut count, &mut kind) };
        ensure!(
            kind == sys::SPA_CHOICE_None && count == 1,
            "PipeWire DRM device id is not fixed"
        );
        // SAFETY: one complete element is present in this live choice.
        value = unsafe { Pod::from_raw(child) };
    }
    let bytes = value
        .get_bytes()
        .context("PipeWire DRM device id is not Bytes")?;
    Ok(Some(u64::from_ne_bytes(
        bytes
            .try_into()
            .context("PipeWire DRM device id is not a Linux dev_t")?,
    )))
}

#[derive(Clone, Copy)]
pub(super) enum Identity {
    Single,
    Matched(u64),
}

fn offer_identity(
    sole: bool,
    advertised: &[u64],
    same_device: impl Fn(u64) -> bool,
) -> Option<Identity> {
    if let Some(&id) = advertised.iter().find(|&&id| same_device(id)) {
        Some(Identity::Matched(id))
    } else if advertised.is_empty() && sole {
        Some(Identity::Single)
    } else {
        None
    }
}

pub(super) struct Importer {
    gpu: Gpu,
    device_path: std::path::PathBuf,
    driver: String,
    bgra: Vec<Modifier>,
    rgba: Vec<Modifier>,
    staging: Option<wgpu::Buffer>,
    // The raw encoder does not track these resources. Keep both alive if a
    // submission panics or its fence times out; never destroy them in flight.
    pending: Option<(Arc<Imported>, wgpu::Buffer)>,
}

impl Importer {
    pub fn probe() -> Option<Self> {
        if !enabled() {
            return None;
        }
        // A headless/test machine with no render node does not need a GPU
        // just to capture a MemFd or LINEAR stream.
        if !Path::new("/sys/class/drm")
            .read_dir()
            .ok()?
            .flatten()
            .any(|e| e.file_name().as_encoded_bytes().starts_with(b"renderD"))
        {
            return None;
        }
        let gpu = Gpu::for_platform(&crate::LinuxPlatform).ok()?;
        if !gpu
            .device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        {
            return None;
        }
        // SAFETY: Vulkan guard and borrowed instance outlive the queries;
        // ash dispatchable handles are repr(transparent) native pointers.
        let hal = unsafe { gpu.device.as_hal::<Vulkan>() }?;
        let handle = hal.raw_physical_device();
        let phys = std::ptr::from_ref(&handle).cast();
        let fp = hal.shared_instance().raw_instance().fp_v1_1();
        let (mut major, mut minor) = (0, 0);
        if unsafe {
            iso_tiled_render_node(
                phys,
                fp.get_physical_device_properties2 as _,
                &mut major,
                &mut minor,
            )
        } == 0
        {
            return None;
        }
        let device = std::fs::canonicalize(format!("/sys/dev/char/{major}:{minor}/device")).ok()?;
        let driver = std::fs::canonicalize(device.join("driver"))
            .ok()?
            .file_name()?
            .to_str()?
            .to_owned();
        let mods = |bgra| {
            let mut out = vec![Modifier::default(); 64];
            // SAFETY: the C query writes at most cap initialized Modifier records.
            let count = unsafe {
                iso_tiled_modifiers(
                    phys,
                    fp.get_physical_device_format_properties2 as _,
                    fp.get_physical_device_image_format_properties2 as _,
                    bgra,
                    out.as_mut_ptr(),
                    out.len() as u32,
                )
            };
            out.truncate(count as usize);
            out
        };
        let (bgra, rgba) = (mods(1), mods(0));
        drop(hal);
        if bgra.is_empty() && rgba.is_empty() {
            return None;
        }
        Some(Self {
            gpu,
            device_path: device,
            driver,
            bgra,
            rgba,
            staging: None,
            pending: None,
        })
    }

    pub fn identity(&self, advertised: &[u64]) -> Option<Identity> {
        // A producer can name either the primary or render node. Compare
        // their physical sysfs devices; bind the offer to the producer's id.
        offer_identity(self.sole_gpu(), advertised, |id| self.same_device(id))
    }

    pub fn same_device(&self, id: u64) -> bool {
        let (major, minor) = (libc::major(id), libc::minor(id));
        let node = std::path::PathBuf::from(format!("/sys/dev/char/{major}:{minor}"));
        std::fs::canonicalize(node.join("subsystem"))
            .is_ok_and(|s| s == Path::new("/sys/class/drm"))
            && std::fs::canonicalize(node.join("device")).is_ok_and(|d| d == self.device_path)
    }

    fn sole_gpu(&self) -> bool {
        let Ok(entries) = Path::new("/sys/class/drm").read_dir() else {
            return false;
        };
        let mut found = false;
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return false;
            };
            let gpu = ["card", "renderD"].iter().any(|p| {
                name.strip_prefix(p)
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            });
            if gpu {
                if !std::fs::canonicalize(entry.path().join("device"))
                    .is_ok_and(|d| d == self.device_path)
                {
                    return false;
                }
                found = true;
            }
        }
        found
    }
    pub fn modifiers(&self, pixels: VideoFormat) -> &[Modifier] {
        match pixels {
            VideoFormat::BGRA | VideoFormat::BGRx => &self.bgra,
            VideoFormat::RGBA | VideoFormat::RGBx => &self.rgba,
            _ => &[],
        }
    }

    pub fn matches_fd(&self, fd: BorrowedFd<'_>, identity: Identity) -> bool {
        if let Identity::Matched(id) = identity {
            return self.same_device(id);
        }
        // exp_name is a driver name, NOT a physical-device identity. This
        // branch is only reachable when all DRM nodes resolve to one GPU.
        self.sole_gpu()
            && std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))
                .ok()
                .is_some_and(|s| {
                    s.lines()
                        .filter_map(|l| l.strip_prefix("exp_name:"))
                        .map(str::trim)
                        .any(|exporter| {
                            exporter == self.driver
                                || (self.driver == "nvidia"
                                    && matches!(exporter, "nvidia-drm" | "NVIDIA"))
                        })
                })
    }

    pub fn import(
        &self,
        fd: BorrowedFd<'_>,
        plane: Plane,
        pixels: VideoFormat,
        (w, h): (u32, u32),
    ) -> Result<Imported> {
        ensure!(
            self.modifiers(pixels)
                .iter()
                .any(|m| m.value == plane.modifier && w <= m.width && h <= m.height),
            "unoffered DRM format/modifier/extent"
        );
        ensure!(
            plane.stride >= u64::from(w) * 4,
            "invalid tiled DMA-BUF stride"
        );
        // SAFETY: SEEK_END is the exporter's size query. Modifier-specific
        // plane layout validation is done by Vulkan, not by linear arithmetic.
        let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        ensure!(
            plane.fits(h, size),
            "tiled plane offset + stride × height exceeds the DMA-BUF"
        );
        let desc = wgpu::hal::TextureDescriptor {
            label: Some("portal DMA-BUF"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: match pixels {
                VideoFormat::BGRA | VideoFormat::BGRx => wgpu::TextureFormat::Bgra8Unorm,
                _ => wgpu::TextureFormat::Rgba8Unorm,
            },
            usage: wgpu::TextureUses::COPY_SRC,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        // SAFETY: validated against the exact supported single-plane pair;
        // Vulkan consumes only our duplicate, never PipeWire's descriptor.
        let hal = unsafe { self.gpu.device.as_hal::<Vulkan>() }.context("Vulkan device lost")?;
        let texture = unsafe {
            hal.texture_from_dmabuf_fd(
                fd.try_clone_to_owned()?,
                &desc,
                plane.modifier,
                plane.stride,
                plane.offset,
            )
        }
        .context("importing tiled screen DMA-BUF")?;
        // We retain the native image rather than putting it in the wgpu
        // tracker. Balance the raw texture accounting with destroy_texture.
        unsafe { hal.add_raw_texture(&texture) };
        Ok(Imported {
            device: self.gpu.device.clone(),
            texture: Some(texture),
            plane,
        })
    }

    pub fn read<T>(
        &mut self,
        image: &Arc<Imported>,
        (w, h): (u32, u32),
        waited: &mut Option<Duration>,
        consume: impl FnOnce(&[u8]) -> T,
    ) -> Result<T> {
        let size = u64::from(w) * u64::from(h) * 4;
        let device = &self.gpu.device;
        if self.staging.as_ref().is_none_or(|b| b.size() != size) {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tiled screen readback"),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: true,
            });
            buffer.unmap(); // wgpu must not insert a deferred zero-fill after our native copy.
            // unmap() queues a staging upload on some memory types. Flush it
            // once, before handing this allocation to the raw command stream.
            let init = self.gpu.queue.submit([]);
            timed(waited, || {
                device.poll(wgpu::PollType::Wait {
                    submission_index: Some(init),
                    timeout: Some(GPU_WAIT),
                })
            })
            .context("waiting for tiled staging initialization")?;
            self.staging = Some(buffer);
        }
        let buffer = self.staging.as_ref().unwrap();
        // SAFETY: both resources are from this device. Only the specified
        // native image is transferred; wgpu owns encoding and submission.
        let commands = raw_commands(device, buffer, |hal, dst, cmd| unsafe {
            let fp = hal.raw_device().fp_v1_0();
            let src = image.texture.as_ref().unwrap().raw_handle();
            iso_tiled_copy(
                std::ptr::from_ref(&cmd.raw_handle()).cast(),
                fp.cmd_pipeline_barrier as _,
                fp.cmd_copy_image_to_buffer as _,
                hal.queue_family_index(),
                std::ptr::from_ref(&src).cast(),
                std::ptr::from_ref(&dst.raw_handle()).cast(),
                w,
                h,
            );
        })?;
        self.pending = Some((Arc::clone(image), buffer.clone()));
        let submission = self.gpu.queue.submit([commands]);
        timed(waited, || {
            device.poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(GPU_WAIT),
            })
        })
        .context("waiting for tiled copy and release to the exporter")?;
        self.pending = None;
        // The buffer is untracked by wgpu for this copy. Do not request CPU
        // mapping (including cache invalidation) until the native write ends.
        let (tx, rx) = mpsc::sync_channel(1);
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        timed(waited, || -> Result<()> {
            device.poll(wgpu::PollType::Poll)?;
            rx.recv_timeout(GPU_WAIT)
                .context("mapping completed tiled copy")??;
            Ok(())
        })?;
        let mapped = buffer.slice(..).get_mapped_range()?;
        let result = consume(&mapped);
        drop(mapped);
        buffer.unmap();
        Ok(result)
    }

    pub fn in_flight(&self) -> bool {
        self.pending.is_some()
    }

    pub fn retire(&mut self) {
        if let Some(resources) = self.pending.take() {
            // On a lost/wedged device the callback may be discarded instead
            // of running. Pin this one uncertain submission rather than free
            // native memory that may still be accessed by the GPU.
            let keep = std::mem::ManuallyDrop::new(resources);
            self.gpu
                .queue
                .on_submitted_work_done(move || drop(std::mem::ManuallyDrop::into_inner(keep)));
        }
    }
}

// wgpu-core forbids mixing its commands with raw Vulkan on one encoder.
fn raw_commands(
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    record: impl FnOnce(
        &wgpu::hal::vulkan::Device,
        &wgpu::hal::vulkan::Buffer,
        &mut wgpu::hal::vulkan::CommandEncoder,
    ),
) -> Result<wgpu::CommandBuffer> {
    let mut encoder = device.create_command_encoder(&Default::default());
    // SAFETY: the handles are borrowed from the same device. `record` only
    // appends native commands to this active encoder; wgpu ends and submits it.
    unsafe {
        let hal = device.as_hal::<Vulkan>().context("not Vulkan")?;
        let dst = buffer.as_hal::<Vulkan>().context("not a Vulkan buffer")?;
        encoder.as_hal_mut::<Vulkan, _, _>(|cmd| -> Result<()> {
            let cmd = cmd.context("no Vulkan command encoder")?;
            record(&hal, &dst, cmd);
            iso_tiled_transfer_to_host(
                std::ptr::from_ref(&cmd.raw_handle()).cast(),
                hal.raw_device().fp_v1_0().cmd_pipeline_barrier as _,
            );
            Ok(())
        })?;
    }
    Ok(encoder.finish())
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct Plane {
    pub fd: i32,
    pub modifier: u64,
    pub stride: u64,
    pub offset: u64,
}
impl Plane {
    fn fits(self, height: u32, size: i64) -> bool {
        size > 0
            && height > 0
            && self
                .stride
                .checked_mul(u64::from(height))
                .and_then(|n| self.offset.checked_add(n))
                .is_some_and(|end| end <= size as u64)
    }
}
pub(super) struct Imported {
    device: wgpu::Device,
    texture: Option<wgpu::hal::vulkan::Texture>,
    pub plane: Plane,
}
impl Drop for Imported {
    fn drop(&mut self) {
        // SAFETY: in-flight raw submissions retain an Arc to the image until
        // completion, including when the capture is torn down after a timeout.
        if let Some(hal) = unsafe { self.device.as_hal::<Vulkan>() } {
            unsafe {
                hal.destroy_texture(self.texture.take().unwrap());
            }
        }
    }
}

pub(super) fn wait_writer(fd: BorrowedFd<'_>) -> Result<()> {
    #[repr(C)]
    struct Export {
        flags: u32,
        fd: i32,
    }
    let mut export = Export { flags: 1, fd: -1 }; // DMA_BUF_SYNC_READ: wait on the last writer
    // SAFETY: DMA_BUF_IOCTL_EXPORT_SYNC_FILE fills fd on success.
    if unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            libc::_IOWR::<Export>(b'b'.into(), 2),
            &mut export,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error()).context("export DMA-BUF acquire fence");
    }
    // SAFETY: the successful ioctl returned a new owned sync-file fd; the
    // guard is its sole owner and closes it on every exit from this wait.
    let fence = unsafe { OwnedFd::from_raw_fd(export.fd) };
    let mut p = libc::pollfd {
        fd: fence.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as i32;
        // SAFETY: poll borrows one initialized pollfd and owns no descriptor.
        let rc = unsafe { libc::poll(&mut p, 1, ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted && ms > 0 {
                continue;
            }
            return Err(err.into());
        }
        ensure!(
            rc == 1 && p.revents == libc::POLLIN,
            "DMA-BUF writer fence failed or timed out"
        );
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entire_pitched_plane_must_fit() {
        let mut p = Plane {
            fd: 3,
            modifier: 1,
            stride: 256,
            offset: 17,
        };
        assert!(p.fits(4, 1041));
        assert!(!p.fits(4, 1040));
        assert!(!p.fits(4, 273));
        p.stride = u64::MAX;
        assert!(!p.fits(2, i64::MAX));
        p.stride = 1;
        p.offset = u64::MAX;
        assert!(!p.fits(1, i64::MAX));
    }

    #[test]
    fn raw_encoder_submits_and_reads_back_first_and_reused_frames() -> Result<()> {
        // SAFETY: no-argument build capability query. Minimal builds have
        // neither the import path nor its native transfer-to-host barrier.
        if unsafe { iso_tiled_available() } == 0 {
            eprintln!("optional native Vulkan import not built");
            return Ok(());
        }
        let gpu = Gpu::new()?;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 1024,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        buffer.unmap();
        let init = gpu.queue.submit([]);
        gpu.device.poll(wgpu::PollType::Wait {
            submission_index: Some(init),
            timeout: Some(GPU_WAIT),
        })?;
        for color in [0xff2255aau32, 0xff8833cc] {
            // SAFETY: this transfer-destination buffer is unmapped and large
            // enough for the fill. `raw_commands` ends the borrowed encoder.
            let cmd = raw_commands(&gpu.device, &buffer, |hal, dst, cmd| unsafe {
                hal.raw_device().cmd_fill_buffer(
                    cmd.raw_handle(),
                    dst.raw_handle(),
                    0,
                    1024,
                    color,
                );
            })?;
            let index = gpu.queue.submit([cmd]);
            gpu.device.poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: Some(GPU_WAIT),
            })?;
            let (tx, rx) = mpsc::channel();
            buffer.map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = tx.send(result);
            });
            gpu.device.poll(wgpu::PollType::Poll)?;
            rx.recv_timeout(GPU_WAIT)??;
            assert!(
                buffer
                    .slice(..)
                    .get_mapped_range()?
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| *p == color.to_ne_bytes())
            );
            buffer.unmap();
        }
        Ok(())
    }
    #[test]
    fn offers_only_for_a_verified_device_or_single_gpu() {
        assert!(matches!(
            offer_identity(true, &[], |id| id == 12),
            Some(Identity::Single)
        ));
        assert!(offer_identity(false, &[], |id| id == 12).is_none());
        assert!(offer_identity(true, &[99], |id| id == 12).is_none());
        assert!(matches!(
            offer_identity(false, &[99, 12], |id| id == 12),
            Some(Identity::Matched(12))
        ));
    }
}
