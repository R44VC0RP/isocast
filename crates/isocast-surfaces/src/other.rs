//! Systems without shared surfaces yet (Windows will share D3D12
//! resources; see docs/shared-frames.md).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

use crate::Token;

const UNSUPPORTED: &str = "shared GPU surfaces are only available on macOS so far";

/// A shared picture in GPU memory (not available on this system).
#[derive(Clone, Debug)]
pub struct Surface(());

impl Surface {
    pub fn rgba(_width: u32, _height: u32) -> Result<Self> {
        bail!(UNSUPPORTED)
    }

    pub fn width(&self) -> u32 {
        0
    }

    pub fn height(&self) -> u32 {
        0
    }

    pub fn id(&self) -> u32 {
        0
    }

    #[cfg(feature = "wgpu")]
    pub fn texture(
        &self,
        _device: &wgpu::Device,
        _usage: wgpu::TextureUsages,
    ) -> Result<wgpu::Texture> {
        bail!(UNSUPPORTED)
    }
}

/// The host clock now, in nanoseconds.
pub fn host_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

/// The daemon's side of handing surfaces over (not available here).
pub struct Exporter(());

impl Exporter {
    pub fn shared() -> Result<&'static Exporter> {
        bail!(UNSUPPORTED)
    }

    pub fn service(&self) -> &str {
        ""
    }

    pub fn offer(&self, _surfaces: Vec<Surface>) -> Token {
        Token([0; 16])
    }

    pub fn withdraw(&self, _token: Token) {}
}

pub fn fetch(_service: &str, _token: Token, _timeout: Duration) -> Result<Vec<Surface>> {
    bail!(UNSUPPORTED)
}
