//! Bounded writable BGRA output, sharing the capture/encoder D3D11 device.
//! GPU drawing uses the native allocation directly; only CPU-generated
//! pictures need an upload. There is no GPU readback or render-output copy.

use anyhow::{Context, Result, bail};
use isocast_engine::FramePool;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION, ID3D11DeviceContext4, ID3D11Texture2D,
};

use crate::{Frame, GraphicsDevice, MAX_POOL_FRAMES};

/// Up to 32 owned BGRA8 textures. A frame and all its consumers must finish
/// before its slot is reused. GPU producers must finish their `GpuCompletion`
/// before publishing a picture to native consumers; the frame's D3D11 fence
/// covers capture/upload work, not GPU rendering.
pub struct OutputPool {
    device: GraphicsDevice,
    width: u32,
    height: u32,
    stride: u32,
    len: usize,
    frames: Vec<Frame>,
    staging: Vec<u8>,
    failed: bool,
}

impl OutputPool {
    pub fn new(device: GraphicsDevice, width: u32, height: u32) -> Result<Self> {
        if width == 0
            || height == 0
            || width > D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION
            || height > D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION
        {
            bail!(
                "output dimensions {width}x{height}: width and height must be 1..={D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION}"
            );
        }
        let stride = width.checked_mul(4).context("output row size overflow")?;
        let len = (stride as usize)
            .checked_mul(height as usize)
            .filter(|len| *len <= isize::MAX as usize)
            .context("output byte size overflow")?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(MAX_POOL_FRAMES)
            .context("could not allocate output frame slots")?;
        Ok(Self {
            device,
            width,
            height,
            stride,
            len,
            frames,
            staging: Vec::new(),
            failed: false,
        })
    }

    fn slot(&mut self) -> Result<Option<usize>> {
        if self.failed {
            bail!("output pool is unusable after a failed D3D11 upload; create a new pool");
        }
        if let Some(index) = self.frames.iter().position(Frame::is_idle_output) {
            return Ok(Some(index));
        }
        if self.frames.len() == MAX_POOL_FRAMES {
            return Ok(None);
        }
        let frame = Frame::new_output(self.device.clone(), self.width, self.height)?;
        self.frames.push(frame);
        Ok(Some(self.frames.len() - 1))
    }

    pub(crate) fn render(
        &mut self,
        draw: impl FnOnce(&ID3D11DeviceContext4, &ID3D11Texture2D) -> Result<()>,
    ) -> Result<Option<Frame>> {
        let Some(index) = self.slot()? else {
            return Ok(None);
        };
        if let Err(error) = self.frames[index].render(draw) {
            self.failed = true;
            return Err(error);
        }
        Ok(Some(self.frames[index].clone()))
    }
}

impl FramePool for OutputPool {
    type Frame = Frame;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Frame>> {
        let Some(index) = self.slot()? else {
            return Ok(None);
        };
        if self.staging.is_empty() {
            self.staging
                .try_reserve_exact(self.len)
                .context("could not allocate BGRA output staging bytes")?;
            self.staging.resize(self.len, 0);
        }
        // No device mutex is held while client code runs, and a full pool
        // never invokes it. The single CPU allocation is reused on each draw.
        draw(&mut self.staging, self.stride as usize);
        if let Err(error) = self.frames[index].upload(&self.staging) {
            self.failed = true;
            return Err(error);
        }
        Ok(Some(self.frames[index].clone()))
    }

    fn take(&mut self) -> Result<Option<Frame>> {
        // Pixels are undefined. Do not clear the retained ready value: an
        // earlier upload may still be pending, and the DX12 target must wait
        // for it before rendering. No CPU wait, upload, copy or flush here.
        Ok(self.slot()?.map(|index| self.frames[index].clone()))
    }
}
