//! Converting a drawn picture to 8-bit 4:2:0 Y'CbCr on the GPU, which is
//! what the encoders take: they then don't convert each frame themselves,
//! and a frame is 1.5 bytes a pixel instead of BGRA's 4.

use crate::gpu::Gpu;

/// Rec. 709 in video range, the way the files are tagged: Y' from 16 to 235
/// and Cb, Cr from 16 to 240 (8-bit codes); chroma is the average of each
/// 2×2 block of pixels.
const SHADER: &str = r#"
@group(0) @binding(0) var picture: texture_2d<f32>;

struct Varyings {
    @builtin(position) position: vec4<f32>,
}

// One triangle that covers the target.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> Varyings {
    let corner = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: Varyings;
    out.position = vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

const KR: f32 = 0.2126;
const KB: f32 = 0.0722;

fn luma(rgb: vec3<f32>) -> f32 {
    return dot(rgb, vec3<f32>(KR, 1.0 - KR - KB, KB));
}

fn texel(at: vec2<i32>) -> vec3<f32> {
    let last = vec2<i32>(textureDimensions(picture)) - vec2<i32>(1);
    return textureLoad(picture, clamp(at, vec2<i32>(0), last), 0).rgb;
}

@fragment
fn fs_luma(in: Varyings) -> @location(0) vec4<f32> {
    let y = luma(texel(vec2<i32>(in.position.xy)));
    return vec4<f32>((16.0 + 219.0 * y) / 255.0, 0.0, 0.0, 1.0);
}

@fragment
fn fs_chroma(in: Varyings) -> @location(0) vec4<f32> {
    let at = vec2<i32>(in.position.xy) * 2;
    let rgb = (texel(at) + texel(at + vec2<i32>(1, 0)) + texel(at + vec2<i32>(0, 1))
        + texel(at + vec2<i32>(1, 1))) * 0.25;
    let y = luma(rgb);
    let cb = (rgb.b - y) / (2.0 * (1.0 - KB));
    let cr = (rgb.r - y) / (2.0 * (1.0 - KR));
    return vec4<f32>((vec2<f32>(128.0) + 224.0 * vec2<f32>(cb, cr)) / 255.0, 0.0, 1.0);
}
"#;

/// The passes that convert a picture into a frame's Y'CbCr planes.
pub struct ToYCbCr {
    layout: wgpu::BindGroupLayout,
    luma: wgpu::RenderPipeline,
    chroma: wgpu::RenderPipeline,
}

impl ToYCbCr {
    pub fn new(gpu: &Gpu) -> Self {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("to ycbcr"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("to ycbcr"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("to ycbcr"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = |entry: &str, format: wgpu::TextureFormat| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Self {
            luma: pipeline("fs_luma", wgpu::TextureFormat::R8Unorm),
            chroma: pipeline("fs_chroma", wgpu::TextureFormat::Rg8Unorm),
            layout,
        }
    }

    /// What the passes read: `picture`, an opaque RGB texture (any channel
    /// order its format names). Made once per picture texture.
    pub fn bind(&self, gpu: &Gpu, picture: &wgpu::Texture) -> wgpu::BindGroup {
        let view = picture.create_view(&Default::default());
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("to ycbcr"),
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        })
    }

    /// Encode converting the picture `bound` names into a frame's `luma`
    /// (an `R8Unorm` view, the picture's size) and `chroma` (`Rg8Unorm`,
    /// half its size) planes.
    pub fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        bound: &wgpu::BindGroup,
        luma: &wgpu::TextureView,
        chroma: &wgpu::TextureView,
    ) {
        for (view, pipeline) in [(luma, &self.luma), (chroma, &self.chroma)] {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("to ycbcr"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Every texel is written; clearing costs nothing on
                        // tile-based GPUs, unlike loading.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bound, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}
