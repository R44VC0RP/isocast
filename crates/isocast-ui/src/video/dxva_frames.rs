//! Decoded DXVA pictures straight into the window's pictures, never
//! through the CPU: the D3D11 video processor converts each NV12 picture
//! (BT.709 limited range, what the engine's encoders write) to RGBA in a
//! ring of surfaces shared with the window's DX12 renderer, which shows
//! them as it shows the local engine's shared frames (the DirectComposition
//! layers, or Slint images).
//!
//! The decoder's D3D11 device must be on the renderer's adapter
//! ([`renderer_luid`]). A picture is handed over once the GPU has finished
//! writing it (a D3D11 fence); its slot is reused only after the
//! renderer's queue is done reading it (`isocast_client::shared`).

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use anyhow::{Context, Result, ensure};
use isocast_client::shared::{LocalRing, SharedFrame, Surface};
use slint::wgpu_30::wgpu;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface;

/// Pictures the window may hold at once (shown, the one before, one on its
/// way) plus one being written.
const SLOTS: usize = 4;

/// Full-range RGB (`RGB_Range`, bit 1, 0 = 0–255).
const OUTPUT_COLOURS: u32 = 0;
/// Conversions failing in a row before the decoder gives up on them (one
/// failing now and then rebuilds the processor and goes on).
const MAX_FAILURES: u32 = 3;

/// A YCbCr stream's colours as D3D11's colour-space bits: `YCbCr_Matrix`
/// (bit 2) 0 = BT.601, 1 = BT.709; `Nominal_Range` (bits 4–5) 1 = 16–235,
/// 2 = 0–255. The engine's encoders write BT.709 studio range.
pub fn input_colours(bt601: bool, full_range: bool) -> u32 {
    (u32::from(!bt601) << 2) | (if full_range { 2 } else { 1 } << 4)
}

/// The adapter the window renders on (none: no renderer registered yet).
pub fn renderer_luid() -> Option<(u32, i32)> {
    let (device, _) = isocast_client::shared::rendering_gpu()?;
    let raw = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }?;
    let luid = unsafe { raw.raw_device().GetAdapterLuid() };
    Some((luid.LowPart, luid.HighPart))
}

pub struct DxvaFrames {
    renderer: wgpu::Device,
    device: ID3D11Device1,
    context: ID3D11DeviceContext4,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    fence: ID3D11Fence,
    fenced: u64,
    event: Event,
    target: Option<Target>,
    /// Conversions that failed in a row, and pictures skipped for want of
    /// a free slot (since last asked).
    failures: u32,
    skipped: u32,
}

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The processor and ring for one input and output size.
struct Target {
    input: (u32, u32),
    output: (u32, u32),
    colours: u32,
    ring: LocalRing,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
    /// Input views by decoder texture and array slice (the decoder cycles
    /// through a fixed pool).
    inputs: HashMap<(usize, u32), ID3D11VideoProcessorInputView>,
}

impl DxvaFrames {
    /// For pictures decoded on `device` (on the renderer's adapter).
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let (renderer, _) = isocast_client::shared::rendering_gpu()
            .context("no renderer registered for shared pictures")?;
        unsafe {
            let context = device.GetImmediateContext()?;
            let mut fence: Option<ID3D11Fence> = None;
            device
                .cast::<ID3D11Device5>()
                .context("D3D11 fences need Windows 10 1703")?
                .CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence)?;
            let event = windows_sys::Win32::System::Threading::CreateEventW(
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
            );
            ensure!(!event.is_null(), "could not make an event");
            Ok(Self {
                renderer,
                device: device.cast()?,
                context: context.cast()?,
                video: device.cast()?,
                video_context: context.cast()?,
                fence: fence.context("no D3D11 fence")?,
                fenced: 0,
                event: Event(HANDLE(event)),
                target: None,
                failures: 0,
                skipped: 0,
            })
        }
    }

    /// Slice `slice` of the decoder's `texture`, its top left
    /// `width`×`height` the picture, coded with `colours`
    /// ([`input_colours`]), as an RGBA frame (none: every slot is still in
    /// use, so this picture is skipped). After a failure the next picture
    /// starts afresh (a new processor and ring).
    pub fn convert(
        &mut self,
        texture: &ID3D11Texture2D,
        slice: u32,
        size: (u32, u32),
        colours: u32,
    ) -> Result<Option<SharedFrame>> {
        let result = self.convert_once(texture, slice, size, colours);
        match &result {
            Ok(frame) => {
                self.failures = 0;
                self.skipped += u32::from(frame.is_none());
            }
            Err(_) => {
                self.failures += 1;
                self.target = None;
            }
        }
        result
    }

    /// Whether conversions keep failing: read pictures back instead.
    pub fn failing(&self) -> bool {
        self.failures >= MAX_FAILURES
    }

    /// Pictures skipped for want of a free slot since last asked.
    pub fn take_skipped(&mut self) -> u32 {
        std::mem::take(&mut self.skipped)
    }

    /// The decoder was made anew: its old textures' input views go.
    pub fn forget_inputs(&mut self) {
        if let Some(target) = &mut self.target {
            target.inputs.clear();
        }
    }

    fn convert_once(
        &mut self,
        texture: &ID3D11Texture2D,
        slice: u32,
        (width, height): (u32, u32),
        colours: u32,
    ) -> Result<Option<SharedFrame>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let input = (desc.Width, desc.Height);
        if self
            .target
            .as_ref()
            .is_none_or(|t| t.input != input || t.output != (width, height) || t.colours != colours)
        {
            self.target = None;
            self.target = Some(self.target_for(input, (width, height), colours)?);
        }
        let target = self.target.as_mut().unwrap();
        let Some(slot) = target.ring.take()? else {
            return Ok(None);
        };
        // Given back (dropped) if anything below fails.
        let frame = target
            .ring
            .frame(slot, isocast_client::shared::host_now(), None);
        unsafe {
            let key = (texture.as_raw() as usize, slice);
            if !target.inputs.contains_key(&key) {
                if target.inputs.len() >= 64 {
                    target.inputs.clear();
                }
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: slice,
                        },
                    },
                };
                let mut view = None;
                self.video.CreateVideoProcessorInputView(
                    texture,
                    &target.enumerator,
                    &desc,
                    Some(&mut view),
                )?;
                target
                    .inputs
                    .insert(key, view.context("no video processor input view")?);
            }
            let source = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            let processor = &target.processor;
            self.video_context
                .VideoProcessorSetStreamSourceRect(processor, 0, true, Some(&source));
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(target.inputs[&key].clone())),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                processor,
                &target.outputs[slot as usize],
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.context("the video processor failed")?;
            // Handed over only once the GPU is done writing it.
            self.fenced += 1;
            self.context.Signal(&self.fence, self.fenced)?;
            self.context.Flush();
            self.fence.SetEventOnCompletion(self.fenced, self.event.0)?;
            ensure!(
                WaitForSingleObject(self.event.0, 1000) == WAIT_OBJECT_0,
                "the GPU took over a second to convert a picture"
            );
        }
        Ok(Some(frame))
    }

    fn target_for(&self, input: (u32, u32), output: (u32, u32), colours: u32) -> Result<Target> {
        unsafe {
            let rate = DXGI_RATIONAL {
                Numerator: 30,
                Denominator: 1,
            };
            let enumerator = self
                .video
                .CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputFrameRate: rate,
                    InputWidth: input.0,
                    InputHeight: input.1,
                    OutputFrameRate: rate,
                    OutputWidth: output.0,
                    OutputHeight: output.1,
                    Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                })
                .context("no video processor for these sizes")?;
            let support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_R8G8B8A8_UNORM)?;
            ensure!(
                support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0,
                "the video processor can't write RGBA"
            );
            let processor = self.video.CreateVideoProcessor(&enumerator, 0)?;
            let vc = &self.video_context;
            vc.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            vc.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: colours },
            );
            vc.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: OUTPUT_COLOURS,
                },
            );
            // The picture as it is: no driver "enhancements".
            vc.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            vc.VideoProcessorSetOutputAlphaFillMode(
                &processor,
                D3D11_VIDEO_PROCESSOR_ALPHA_FILL_MODE_OPAQUE,
                0,
            );
            let whole = RECT {
                left: 0,
                top: 0,
                right: output.0 as i32,
                bottom: output.1 as i32,
            };
            vc.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&whole));
            vc.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole));
            let surfaces = (0..SLOTS)
                .map(|_| Surface::rgba_on(&self.renderer, output.0, output.1))
                .collect::<Result<Vec<_>>>()?;
            let mut outputs = Vec::with_capacity(SLOTS);
            for surface in &surfaces {
                let texture: ID3D11Texture2D = self
                    .device
                    .OpenSharedResource1(surface.shared_handle())
                    .context("could not open a shared picture surface on the decoder's device")?;
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                self.video.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &desc,
                    Some(&mut view),
                )?;
                outputs.push(view.context("no video processor output view")?);
            }
            Ok(Target {
                input,
                output,
                colours,
                ring: LocalRing::new(surfaces)?,
                enumerator,
                processor,
                outputs,
                inputs: HashMap::new(),
            })
        }
    }
}
