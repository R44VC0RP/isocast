//! The GPU device the window and the preview share. The app creates it and
//! hands it to Slint (`WGPUConfiguration::Manual`), so textures it makes
//! (today the test picture, later the daemon's shared frames) are drawn by
//! Slint's Skia renderer without a copy.

use std::future::Future;
use std::pin::pin;
use std::task::{Context as TaskContext, Poll, Waker};

use anyhow::{Context, Result};
use slint::wgpu_30::wgpu;

#[derive(Clone)]
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        // The platform's own API only: Metal on macOS, DX12 on Windows,
        // Vulkan elsewhere (never GL).
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: if cfg!(windows) {
                wgpu::Backends::DX12
            } else {
                wgpu::Backends::PRIMARY
            },
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        // A remote engine's adapter is on another machine: this one's own
        // (and no connection before the window opens).
        #[cfg(windows)]
        let adapter = if crate::engine::is_local() {
            match crate::daemon::connect() {
                Ok(mut client) => isocast_client::shared::adapter(&mut client, &instance)
                    .context("could not select the daemon's preview adapter")?,
                Err(_) => {
                    // Still open the app (including Help > Report a Bug). Do
                    // not bypass shared surfaces' adapter check on reconnect.
                    isocast_client::shared::local_adapter(&instance)
                        .context("could not select a graphics adapter while the engine was down")?
                }
            }
        } else {
            isocast_client::shared::local_adapter(&instance)
                .context("could not select a graphics adapter")?
        };
        #[cfg(not(windows))]
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))
        .context("no GPU found")?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("isocast-ui"),
            // Preview textures are shared with the daemon. The UI's own small
            // allocations do not need DX12's default 128 MiB heap blocks.
            #[cfg(windows)]
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            // DX12 eagerly allocates the entire descriptor heap. The default
            // million bindings are unnecessary for the UI's shared previews.
            #[cfg(windows)]
            required_limits: wgpu::Limits {
                max_non_sampler_bindings: 65_536,
                ..Default::default()
            },
            ..Default::default()
        }))
        .context("could not open the GPU")?;
        isocast_client::shared::register_gpu(&device, &queue)?;
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
        })
    }

    pub fn slint_configuration(&self) -> slint::wgpu_30::WGPUConfiguration {
        slint::wgpu_30::WGPUConfiguration::Manual {
            instance: self.instance.clone(),
            adapter: self.adapter.clone(),
            device: self.device.clone(),
            queue: self.queue.clone(),
        }
    }
}

/// wgpu's device requests complete at once on native backends.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}
