//! GPU-only BGRA8/RGB10 conversion or qualified NV12 copy/expansion to
//! limited-range Rec.709 NV12/P010 for the MF worker.
//! Ten-bit output is SDR, not HDR. Only an explicitly selected software
//! encoder may call `readback`.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{RECT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface;

use crate::frames::PixelFormat;
use crate::native::Event;
use crate::p010::P010Converter;
use crate::{Frame, GraphicsDevice};

const COMPLETION_TIMEOUT_MS: u32 = 2_000;

pub(crate) struct Converter {
    device: GraphicsDevice,
    context: ID3D11DeviceContext4,
    video: Option<VideoProcessor>,
    p010: Option<P010Converter>,
    fence: ID3D11Fence,
    event: Option<Event>,
    value: u64,
    width: u32,
    height: u32,
    input_format: DXGI_FORMAT,
    output_format: DXGI_FORMAT,
    pending: Option<Pending>,
}

struct Pending {
    _frame: Option<Frame>,
    _source: ID3D11Texture2D,
    _target: ID3D11Texture2D,
}

struct VideoProcessor {
    device: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator1,
    processor: ID3D11VideoProcessor,
}

impl Converter {
    pub(crate) fn with_format(
        device: &GraphicsDevice,
        width: u32,
        height: u32,
        fps: u32,
        input: PixelFormat,
        bit_depth: u8,
    ) -> Result<Self> {
        let input_format = match input {
            PixelFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            PixelFormat::Rgba10 => DXGI_FORMAT_R10G10B10A2_UNORM,
            PixelFormat::Nv12 => DXGI_FORMAT_NV12,
        };
        let output_format = match bit_depth {
            8 => DXGI_FORMAT_NV12,
            10 => DXGI_FORMAT_P010,
            _ => bail!("video.bit_depth: valid values are 8 and 10"),
        };
        if input == PixelFormat::Rgba10 && bit_depth != 10 {
            bail!("video.bit_depth: Rgba10 input requires 10-bit output");
        }
        for (field, size) in [("width", width), ("height", height)] {
            if !(2..=D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION).contains(&size)
                || !size.is_multiple_of(2)
            {
                bail!("video.{field}: NV12/P010 requires an even size in 2..=16384");
            }
        }
        if !(1..=240).contains(&fps) {
            bail!("video.fps: valid range is 1..=240");
        }
        unsafe {
            let context: ID3D11DeviceContext4 = device.raw().GetImmediateContext()?.cast()?;
            let video = if bit_depth == 8 && input != PixelFormat::Nv12 {
                let video: ID3D11VideoDevice = device.raw().cast()?;
                let video_context: ID3D11VideoContext1 = context.cast()?;
                let rate = DXGI_RATIONAL {
                    Numerator: fps,
                    Denominator: 1,
                };
                let enumerator: ID3D11VideoProcessorEnumerator1 = video
                    .CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                        InputFrameRate: rate,
                        InputWidth: width,
                        InputHeight: height,
                        OutputFrameRate: rate,
                        OutputWidth: width,
                        OutputHeight: height,
                        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                    })?
                    .cast()?;
                if enumerator.CheckVideoProcessorFormat(input_format)?
                    & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
                    == 0
                    || enumerator.CheckVideoProcessorFormat(output_format)?
                        & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32
                        == 0
                    || !enumerator
                        .CheckVideoProcessorFormatConversion(
                            input_format,
                            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                            output_format,
                            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                        )?
                        .as_bool()
                {
                    bail!(
                        "D3D11 video processor does not support full Rec.709 {input:?} to limited Rec.709 {bit_depth}-bit YUV"
                    );
                }
                let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
                enumerator.GetVideoProcessorCaps(&mut caps)?;
                if caps.RateConversionCapsCount == 0 || caps.MaxInputStreams == 0 {
                    bail!("D3D11 video processor has no usable stream or rate-conversion mode");
                }
                // Same-rate progressive input needs no deinterlacing or temporal references.
                let processor = video.CreateVideoProcessor(&enumerator, 0)?;
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                };
                video_context.VideoProcessorSetStreamFrameFormat(
                    &processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
                video_context.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                video_context.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
                video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&rect));
                video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect));
                video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect));
                Some(VideoProcessor {
                    device: video,
                    context: video_context,
                    enumerator,
                    processor,
                })
            } else {
                None
            };
            // The NVIDIA video processor treats P010 like normalized 16-bit
            // video: nonzero padding bits, white944 and neutral514 at ten-bit.
            // The shader writes exact limited-range codes with six zero bits.
            let p010 = if bit_depth == 10 {
                Some(P010Converter::new(device, input)?)
            } else {
                None
            };
            let mut fence = None;
            device
                .raw()
                .CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)?;
            Ok(Self {
                device: device.clone(),
                context,
                video,
                p010,
                fence: fence.context("D3D11 returned no conversion fence")?,
                event: Some(Event::new()?),
                value: 0,
                width,
                height,
                input_format,
                output_format,
                pending: None,
            })
        }
    }

    pub(crate) fn create_texture(&self) -> Result<ID3D11Texture2D> {
        self.idle()?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: self.output_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0
                | if self.p010.is_some() {
                    D3D11_BIND_UNORDERED_ACCESS.0
                } else {
                    0
                }) as u32,
            ..Default::default()
        };
        let mut texture = None;
        self.check(
            unsafe {
                self.device
                    .raw()
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            },
            "create YUV conversion texture",
        )?;
        texture.context("D3D11 returned no YUV conversion texture")
    }

    /// Call only on the private encoder worker; successful return means the
    /// GPU conversion is complete. The caller must not reuse a target while
    /// an MFT still owns its sample, or after any submission/completion error.
    pub(crate) fn convert(&mut self, frame: &Frame, target: &ID3D11Texture2D) -> Result<()> {
        self.idle()?;
        if !self.device.owns(frame) || frame.width() != self.width || frame.height() != self.height
        {
            bail!(
                "conversion frame must belong to this GraphicsDevice and match {}x{}",
                self.width,
                self.height
            );
        }
        self.texture_desc(frame.texture(), self.input_format, false)?;
        self.texture_desc(target, self.output_format, true)?;
        if self.input_format == DXGI_FORMAT_NV12 && self.output_format == DXGI_FORMAT_NV12 {
            self.retain(Some(frame.clone()), frame.texture(), target)?;
            self.check(
                unsafe { self.context.Wait(frame.fence(), frame.ready_value()) },
                "wait for captured NV12",
            )?;
            unsafe { self.context.CopyResource(target, frame.texture()) };
            return self.finish();
        }
        if self.p010.is_some() {
            self.retain(Some(frame.clone()), frame.texture(), target)?;
            self.check(
                unsafe { self.context.Wait(frame.fence(), frame.ready_value()) },
                "wait for captured P010 source",
            )?;
            self.p010.as_ref().expect("checked above").convert(
                frame.texture(),
                target,
                self.width,
                self.height,
            )?;
            return self.finish();
        }
        let video = self
            .video
            .as_ref()
            .expect("RGB to NV12 converter owns a video processor");
        let mut input = None;
        let mut output = None;
        unsafe {
            self.check(
                video.device.CreateVideoProcessorInputView(
                    frame.texture(),
                    &video.enumerator,
                    &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV {
                                MipSlice: 0,
                                ArraySlice: 0,
                            },
                        },
                    },
                    Some(&mut input),
                ),
                "create RGB processor input view",
            )?;
            self.check(
                video.device.CreateVideoProcessorOutputView(
                    target,
                    &video.enumerator,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    },
                    Some(&mut output),
                ),
                "create YUV processor output view",
            )?;
        }
        let input = input.context("D3D11 returned no processor input view")?;
        let output = output.context("D3D11 returned no processor output view")?;
        let context = video.context.clone();
        let processor = video.processor.clone();
        self.retain(Some(frame.clone()), frame.texture(), target)?;
        unsafe {
            // Capture already submitted its signal and Flush before publishing.
            // Interleaved immediate-context calls cannot reverse that dependency.
            self.check(
                self.context.Wait(frame.fence(), frame.ready_value()),
                "wait for captured RGB",
            )?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(input)),
                ..Default::default()
            };
            let result =
                context.VideoProcessorBlt(&processor, &output, 0, std::slice::from_ref(&stream));
            // windows-rs represents this COM member as ManuallyDrop, not RAII.
            ManuallyDrop::drop(&mut stream.pInputSurface);
            self.check(result, "convert RGB to YUV")?;
        }
        self.finish()
    }

    pub(crate) fn readback(&mut self, target: &ID3D11Texture2D) -> Result<Vec<u8>> {
        self.idle()?;
        let mut desc = self.texture_desc(target, self.output_format, true)?;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        self.check(
            unsafe {
                self.device
                    .raw()
                    .CreateTexture2D(&desc, None, Some(&mut staging))
            },
            "create YUV readback texture",
        )?;
        let staging = staging.context("D3D11 returned no readback texture")?;
        let row_bytes = self.width as usize
            * if self.output_format == DXGI_FORMAT_P010 {
                2
            } else {
                1
            };
        let rows = (self.height + self.height / 2) as usize;
        let mut bytes = vec![0; row_bytes * rows];
        self.retain(None, target, &staging)?;
        unsafe { self.context.CopyResource(&staging, target) };
        self.finish()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // A completed GPU fence does not guarantee that WDDM has finished
        // preparing the CPU mapping (observed on RTX 5070). Keep this wait
        // bounded instead of making Map block indefinitely or failing a
        // valid software fallback on its first transient STILL_DRAWING.
        let deadline = Instant::now() + Duration::from_millis(u64::from(COMPLETION_TIMEOUT_MS));
        loop {
            let result = unsafe {
                self.context.Map(
                    &staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            };
            if result
                .as_ref()
                .is_err_and(|error| error.code() == DXGI_ERROR_WAS_STILL_DRAWING)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            self.check(result, "map completed YUV readback")?;
            break;
        }
        let pitch = mapped.RowPitch as usize;
        let valid = !mapped.pData.is_null()
            && pitch >= row_bytes
            && pitch
                .checked_mul(rows)
                .is_some_and(|size| size <= isize::MAX as usize);
        if valid {
            // NV12/P010 have height Y rows followed by height/2 interleaved
            // UV rows, both with RowPitch. P010 stores each sample in the high
            // ten bits of a 16-bit word. Copy pixels only, never padding.
            for (row, dest) in bytes.chunks_exact_mut(row_bytes).enumerate() {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        mapped.pData.cast::<u8>().add(row * pitch),
                        dest.as_mut_ptr(),
                        row_bytes,
                    )
                };
            }
        }
        unsafe { self.context.Unmap(&staging, 0) };
        if !valid {
            bail!("D3D11 YUV readback returned a null pointer or invalid row pitch");
        }
        Ok(bytes)
    }

    fn idle(&self) -> Result<()> {
        unsafe { self.device.raw().GetDeviceRemovedReason() }
            .context("D3D11 conversion device lost")?;
        if self.pending.is_some() {
            bail!("D3D11 converter is unusable after an unfinished GPU operation");
        }
        Ok(())
    }

    fn texture_desc(
        &self,
        texture: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        output: bool,
    ) -> Result<D3D11_TEXTURE2D_DESC> {
        let owner: ID3D11Device5 = unsafe { texture.GetDevice() }?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if owner != *self.device.raw()
            || desc.Width != self.width
            || desc.Height != self.height
            || desc.Format != format
            || desc.MipLevels != 1
            || desc.ArraySize != 1
            || desc.SampleDesc.Count != 1
            || desc.SampleDesc.Quality != 0
            || desc.Usage != D3D11_USAGE_DEFAULT
            || desc.CPUAccessFlags != 0
            || (output
                && (desc.MiscFlags != 0 || desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 == 0))
        {
            bail!(
                "conversion texture must be same-device {}x{} {format:?}, single mip/slice/sample and GPU-only (outputs must be private render targets)",
                self.width,
                self.height
            );
        }
        Ok(desc)
    }

    fn retain(
        &mut self,
        frame: Option<Frame>,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
    ) -> Result<()> {
        self.value = self
            .value
            .checked_add(1)
            .filter(|value| *value != u64::MAX)
            .context("D3D11 conversion fence exhausted")?;
        self.pending = Some(Pending {
            _frame: frame,
            _source: source.clone(),
            _target: target.clone(),
        });
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let event = self
            .event
            .as_ref()
            .expect("live converter owns its event")
            .handle();
        unsafe {
            self.check(
                self.context.Signal(&self.fence, self.value),
                "signal conversion fence",
            )?;
            self.context.Flush();
            self.check(
                self.fence.SetEventOnCompletion(self.value, event),
                "register conversion completion",
            )?;
            let status = WaitForSingleObject(event, COMPLETION_TIMEOUT_MS);
            let wait_error = (status == WAIT_FAILED).then(windows::core::Error::from_thread);
            let completed = self.fence.GetCompletedValue();
            // Device removal signals fences with UINT64_MAX, not success.
            self.device
                .raw()
                .GetDeviceRemovedReason()
                .context("D3D11 conversion device lost")?;
            if completed == u64::MAX {
                bail!("D3D11 conversion fence reports device removal");
            }
            match status {
                WAIT_OBJECT_0 if completed >= self.value => {}
                WAIT_TIMEOUT => {
                    bail!("D3D11 conversion did not complete within {COMPLETION_TIMEOUT_MS} ms")
                }
                WAIT_FAILED => {
                    return Err(wait_error.expect("captured above"))
                        .context("wait for conversion completion");
                }
                _ => bail!("D3D11 conversion completion was not confirmed ({status:?})"),
            }
        }
        self.pending = None;
        Ok(())
    }

    fn check<T>(&self, result: windows::core::Result<T>, operation: &'static str) -> Result<T> {
        if result.is_err() {
            unsafe { self.device.raw().GetDeviceRemovedReason() }
                .context("D3D11 conversion device lost")?;
        }
        result.context(operation)
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            // Failed Signal/timeout cannot prove the GPU stopped reading the
            // pool's frame. Like NVENC teardown, quarantine this one operation
            // until process exit; never recycle it or close a registered event.
            eprintln!("D3D11 conversion: unfinished GPU resources retained until process exit");
            std::mem::forget(pending);
            std::mem::forget(self.event.take());
            std::mem::forget(self.fence.clone());
            std::mem::forget(self.device.clone());
        }
    }
}
