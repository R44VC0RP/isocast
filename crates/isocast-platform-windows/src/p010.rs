//! GPU-only SDR Rec.709 RGB conversion or NV12 code expansion to P010.
//! The caller owns frame leases, GPU ordering, completion and failure quarantine.

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{D3D11_SRV_DIMENSION_TEXTURE2D, ID3DInclude};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, s};

use crate::{GraphicsDevice, PixelFormat};

const SHADER: &str = r#"
Texture2D<float4> source : register(t0);
RWTexture2D<float> luma : register(u0);
RWTexture2D<float2> chroma : register(u1);

float y709(float3 rgb) { return dot(rgb, float3(0.2126, 0.7152, 0.0722)); }
float packed_y(float3 rgb) {
    return clamp(round(64.0 + 876.0 * y709(rgb)), 64.0, 940.0) * (64.0 / 65535.0);
}

[numthreads(8, 8, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint width, height;
    source.GetDimensions(width, height);
    uint2 p = thread.xy * 2;
    if (p.x + 1 >= width || p.y + 1 >= height) return;
    float3 a = source.Load(int3(p, 0)).rgb;
    float3 b = source.Load(int3(p + uint2(1, 0), 0)).rgb;
    float3 c = source.Load(int3(p + uint2(0, 1), 0)).rgb;
    float3 d = source.Load(int3(p + uint2(1, 1), 0)).rgb;
    luma[p] = packed_y(a);
    luma[p + uint2(1, 0)] = packed_y(b);
    luma[p + uint2(0, 1)] = packed_y(c);
    luma[p + uint2(1, 1)] = packed_y(d);

    // Left-sited 4:2:0: horizontal [1,2,1]/4, vertical two-row average.
    int left = max(int(p.x) - 1, 0);
    float3 e = source.Load(int3(left, p.y, 0)).rgb;
    float3 f = source.Load(int3(left, p.y + 1, 0)).rgb;
    float3 rgb = (e + f + 2.0 * (a + c) + b + d) * 0.125;
    float y = y709(rgb);
    float2 cbcr = (rgb.br - y) / float2(1.8556, 1.5748);
    float2 q = clamp(round(512.0 + 896.0 * cbcr), 64.0, 960.0);
    // R16 UNORM stores q << 6, not a 16-bit approximation of limited range.
    chroma[thread.xy] = q * (64.0 / 65535.0);
}
"#;

// Preserve the original 8-bit codes and chroma sites: code8 * 4 in ten bits,
// then << 6 in P010. This expands storage, not source precision or gamut.
const NV12_SHADER: &str = r#"
Texture2D<float> source_y : register(t0);
Texture2D<float2> source_uv : register(t1);
RWTexture2D<float> luma : register(u0);
RWTexture2D<float2> chroma : register(u1);

float packed_y(uint2 p) {
    return round(source_y.Load(int3(p, 0)) * 255.0) * (256.0 / 65535.0);
}

[numthreads(8, 8, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint width, height;
    source_y.GetDimensions(width, height);
    uint2 p = thread.xy * 2;
    if (p.x + 1 >= width || p.y + 1 >= height) return;
    luma[p] = packed_y(p);
    luma[p + uint2(1, 0)] = packed_y(p + uint2(1, 0));
    luma[p + uint2(0, 1)] = packed_y(p + uint2(0, 1));
    luma[p + uint2(1, 1)] = packed_y(p + uint2(1, 1));
    chroma[thread.xy] = round(source_uv.Load(int3(thread.xy, 0)) * 255.0)
        * (256.0 / 65535.0);
}
"#;

pub(crate) struct P010Converter {
    device: ID3D11Device3,
    context: ID3D11DeviceContext,
    multithread: ID3D11Multithread,
    shader: ID3D11ComputeShader,
    input_format: DXGI_FORMAT,
}

impl P010Converter {
    pub(crate) fn new(device: &GraphicsDevice, input: PixelFormat) -> Result<Self> {
        let input_format = match input {
            PixelFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            PixelFormat::Rgba10 => DXGI_FORMAT_R10G10B10A2_UNORM,
            PixelFormat::Nv12 => DXGI_FORMAT_NV12,
        };
        let source = if input == PixelFormat::Nv12 {
            NV12_SHADER
        } else {
            SHADER
        };
        unsafe {
            let device: ID3D11Device3 =
                device.raw().cast().context("P010 requires ID3D11Device3")?;
            let context = device
                .GetImmediateContext()
                .context("get P010 immediate context")?;
            let multithread: ID3D11Multithread = context.cast().context("get P010 context lock")?;
            if !multithread.GetMultithreadProtected().as_bool() {
                bail!("P010 requires a multithread-protected immediate context");
            }
            let mut code = None;
            let mut errors = None;
            let result = D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                s!("p010.hlsl"),
                None,
                None::<&ID3DInclude>,
                s!("main"),
                s!("cs_5_0"),
                D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut code,
                Some(&mut errors),
            );
            if let Err(error) = result {
                let detail = errors
                    .map(|blob| {
                        String::from_utf8_lossy(std::slice::from_raw_parts(
                            blob.GetBufferPointer().cast::<u8>(),
                            blob.GetBufferSize(),
                        ))
                        .into_owned()
                    })
                    .unwrap_or_default();
                return Err(error).with_context(|| format!("compile P010 shader: {detail}"));
            }
            let code = code.context("D3DCompile returned no P010 shader bytecode")?;
            let bytes =
                std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize());
            let mut shader = None;
            device
                .CreateComputeShader(bytes, None, Some(&mut shader))
                .context("create P010 compute shader")?;
            Ok(Self {
                device,
                context,
                multithread,
                shader: shader.context("D3D11 returned no P010 compute shader")?,
                input_format,
            })
        }
    }

    /// Only queues work. Retain both textures (including the source frame's
    /// pool lease) until the caller's fence completes, even after an error.
    pub(crate) fn convert(
        &self,
        source: &ID3D11Texture2D,
        target: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<()> {
        for (field, size) in [("width", width), ("height", height)] {
            if !(2..=D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION).contains(&size)
                || !size.is_multiple_of(2)
            {
                bail!("P010 {field}: requires an even size in 2..=16384");
            }
        }
        unsafe {
            for (role, texture, bind, formats) in [
                (
                    "source",
                    source,
                    D3D11_BIND_SHADER_RESOURCE,
                    std::slice::from_ref(&self.input_format),
                ),
                (
                    "target",
                    target,
                    D3D11_BIND_UNORDERED_ACCESS,
                    &[DXGI_FORMAT_P010][..],
                ),
            ] {
                let owner: ID3D11Device3 = texture
                    .GetDevice()
                    .with_context(|| format!("get P010 {role} device"))?
                    .cast()
                    .with_context(|| format!("query P010 {role} device"))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                if owner != self.device
                    || desc.Width != width
                    || desc.Height != height
                    || desc.MipLevels != 1
                    || desc.ArraySize != 1
                    || desc.SampleDesc.Count != 1
                    || desc.SampleDesc.Quality != 0
                    || desc.Usage != D3D11_USAGE_DEFAULT
                    || desc.CPUAccessFlags != 0
                    || desc.BindFlags & bind.0 as u32 == 0
                    || !formats.contains(&desc.Format)
                {
                    bail!(
                        "P010 {role}: requires same-device GPU-only default texture, exact {width}x{height}, one mip/slice/sample, bind {bind:?}, format {formats:?}"
                    );
                }
            }
            // Create every transient view before taking the shared-context lock.
            let (input, input_count) = if self.input_format == DXGI_FORMAT_NV12 {
                (
                    [
                        Some(self.input_plane(source, DXGI_FORMAT_R8_UNORM, 0)?),
                        Some(self.input_plane(source, DXGI_FORMAT_R8G8_UNORM, 1)?),
                    ],
                    2,
                )
            } else {
                let mut input = None;
                self.device
                    .CreateShaderResourceView(source, None, Some(&mut input))
                    .context("create P010 RGB shader resource view")?;
                (
                    [
                        Some(input.context("D3D11 returned no P010 RGB shader resource view")?),
                        None,
                    ],
                    1,
                )
            };
            let output = [
                Some(self.plane(target, DXGI_FORMAT_R16_UNORM, 0)?),
                Some(self.plane(target, DXGI_FORMAT_R16G16_UNORM, 1)?),
            ];
            self.multithread.Enter();
            let _lock = ContextLock(&self.multithread);
            self.context.CSSetShader(&self.shader, None);
            self.context
                .CSSetShaderResources(0, Some(&input[..input_count]));
            self.context
                .CSSetUnorderedAccessViews(0, 2, Some(output.as_ptr()), None);
            self.context
                .Dispatch((width / 2).div_ceil(8), (height / 2).div_ceil(8), 1);
            self.context
                .CSSetUnorderedAccessViews(0, 2, Some([None, None].as_ptr()), None);
            self.context
                .CSSetShaderResources(0, Some(&[None, None][..input_count]));
            self.context.CSSetShader(None, None);
        }
        Ok(())
    }

    fn input_plane(
        &self,
        source: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        plane: u32,
    ) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC1 {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC1_0 {
                Texture2D: D3D11_TEX2D_SRV1 {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    PlaneSlice: plane,
                },
            },
        };
        let mut view = None;
        unsafe {
            self.device
                .CreateShaderResourceView1(source, Some(&desc), Some(&mut view))
        }
        .with_context(|| format!("create NV12 plane {plane} SRV ({format:?})"))?;
        view.context("D3D11 returned no NV12 plane SRV")?
            .cast()
            .context("query NV12 plane SRV base interface")
    }

    fn plane(
        &self,
        target: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        plane: u32,
    ) -> Result<ID3D11UnorderedAccessView> {
        let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC1 {
            Format: format,
            ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC1_0 {
                Texture2D: D3D11_TEX2D_UAV1 {
                    MipSlice: 0,
                    PlaneSlice: plane,
                },
            },
        };
        let mut view = None;
        unsafe {
            self.device
                .CreateUnorderedAccessView1(target, Some(&desc), Some(&mut view))
        }
        .with_context(|| {
            format!(
                "create P010 plane {plane} UAV ({format:?}); driver must support planar UAV writes"
            )
        })?;
        view.context("D3D11 returned no P010 plane UAV")?
            .cast()
            .context("query P010 plane UAV base interface")
    }
}

struct ContextLock<'a>(&'a ID3D11Multithread);

impl Drop for ContextLock<'_> {
    fn drop(&mut self) {
        unsafe { self.0.Leave() };
    }
}
