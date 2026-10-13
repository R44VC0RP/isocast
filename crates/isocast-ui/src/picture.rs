//! A stand-in for the daemon's picture: a moving test pattern drawn on the
//! GPU into an RGBA8 texture at the preview's size in physical pixels, with
//! the selected item drawn where the selection box is. Both come from the
//! same values in the same frame, so a drag shows whether the box and the
//! picture stay together. The frame transport replaces the pattern with
//! the daemon's shared frames.

use slint::wgpu_30::wgpu;

/// What one frame shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scene {
    /// Seconds; the pattern scrolls with it.
    pub time: f32,
    /// The selected item's rect, as fractions of the canvas.
    pub rect: [f32; 4],
}

pub struct TestPicture {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    texture: Option<wgpu::Texture>,
}

/// The texture's format: the one Slint imports.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// The item's fill, and what the drift check looks for.
pub const ITEM_RGB: [u8; 3] = [62, 142, 126];

const SHADER: &str = r#"
struct Uniforms {
    size: vec2<f32>,
    time: f32,
    pad: f32,
    rect: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = pos.xy / u.size;
    // Slate, with soft diagonal bands scrolling to the right.
    var c = vec3<f32>(0.180, 0.200, 0.220);
    let band = fract((uv.x * 1.7778 + uv.y) * 4.0 - u.time * 0.25);
    if (band < 0.5) {
        c = vec3<f32>(0.205, 0.227, 0.250);
    }
    // A grid every sixteenth of the canvas.
    let g = abs(fract(uv * 16.0 + 0.5) - 0.5) * u.size / 16.0;
    if (min(g.x, g.y) < 0.5) {
        c = c + vec3<f32>(0.035);
    }
    // Colour bars along the bottom, sliding.
    if (uv.y > 0.86) {
        let bar = i32(floor(fract(uv.x - u.time * 0.1) * 7.0));
        var bars = array<vec3<f32>, 7>(
            vec3<f32>(0.75, 0.75, 0.75), vec3<f32>(0.75, 0.75, 0.0), vec3<f32>(0.0, 0.75, 0.75),
            vec3<f32>(0.0, 0.75, 0.0), vec3<f32>(0.75, 0.0, 0.75), vec3<f32>(0.75, 0.0, 0.0),
            vec3<f32>(0.0, 0.0, 0.75));
        c = bars[bar] * 0.6;
    }
    // The selected item: verdigris, exactly where the box is.
    let r = u.rect;
    if (uv.x >= r.x && uv.x < r.x + r.z && uv.y >= r.y && uv.y < r.y + r.w) {
        c = vec3<f32>(62.0, 142.0, 126.0) / 255.0;
        // A moving checker inside it, so it visibly plays.
        let local = (uv - r.xy) / r.zw;
        let checker = (i32(floor(local.x * 8.0 + u.time)) + i32(floor(local.y * 4.5))) & 1;
        if (checker == 1 && local.x > 0.08 && local.x < 0.92 && local.y > 0.12 && local.y < 0.88) {
            c = vec3<f32>(86.0, 169.0, 150.0) / 255.0;
        }
    }
    return vec4<f32>(c, 1.0);
}
"#;

impl TestPicture {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test picture"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("test picture"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("test picture"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test picture"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(FORMAT.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test picture"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test picture"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipeline,
            uniforms,
            bind_group,
            texture: None,
        }
    }

    /// Draw a frame at `width`×`height` pixels. Returns a new image to show
    /// when the texture had to be (re)made; otherwise the shown image now
    /// holds the new frame. Submitted on the queue Slint renders with, so
    /// the frame is complete before the window draws it.
    pub fn draw(&mut self, width: u32, height: u32, scene: Scene) -> Option<slint::Image> {
        let (width, height) = (width.clamp(1, 8192), height.clamp(1, 8192));
        let mut replaced = None;
        let stale = self
            .texture
            .as_ref()
            .is_none_or(|t| t.width() != width || t.height() != height);
        if stale {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("preview"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            replaced = Some(texture.clone());
            self.texture = Some(texture);
        }
        let texture = self.texture.as_ref().expect("made above");

        let mut bytes = Vec::with_capacity(32);
        for value in [
            width as f32,
            height as f32,
            scene.time,
            0.0,
            scene.rect[0],
            scene.rect[1],
            scene.rect[2],
            scene.rect[3],
        ] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
        self.queue.write_buffer(&self.uniforms, 0, &bytes);

        let view = texture.create_view(&Default::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("test picture"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test picture"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);

        replaced.map(|texture| {
            slint::Image::try_from(texture).expect("an RGBA8 render target Slint can import")
        })
    }
}
