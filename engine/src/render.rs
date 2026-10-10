//! The GPU compositor.
//!
//! [`Gpu`] owns the wgpu device/queue and the two render pipelines (scene and
//! warp). Each [`OutputGpu`] owns one window's surface, an offscreen "canvas
//! slice" texture, per-layer uniform buffers, and the warp mesh + blend uniform
//! for that output.
//!
//! Per output, each frame runs two passes:
//!   1. **scene** → render every visible layer into the slice texture using the
//!      output's view-projection (so content spanning outputs is clipped per
//!      output automatically);
//!   2. **warp** → sample the slice through the warp mesh and multiply by the
//!      edge-blend mask, writing to the window surface.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt as _;
use winit::window::Window;

use crate::content::Content;
use crate::show::{Output, Show};

/// Offscreen "canvas slice" format. sRGB so shader math stays in linear space.
const SCENE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// Warp mesh resolution (cells per side). Higher = smoother non-linear warps.
const WARP_GRID: u32 = 32;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct SceneUniform {
    mvp: [[f32; 4]; 4],
    opacity: f32,
    _pad: [f32; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct BlendUniform {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    gamma: f32,
    _pad: [f32; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

const QUAD_VERTS: [Vertex; 4] = [
    Vertex {
        pos: [0.0, 0.0],
        uv: [0.0, 0.0],
    },
    Vertex {
        pos: [1.0, 0.0],
        uv: [1.0, 0.0],
    },
    Vertex {
        pos: [0.0, 1.0],
        uv: [0.0, 1.0],
    },
    Vertex {
        pos: [1.0, 1.0],
        uv: [1.0, 1.0],
    },
];
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 2, 1, 3];

pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,

    surface_format: wgpu::TextureFormat,

    scene_pipeline: wgpu::RenderPipeline,
    warp_pipeline: wgpu::RenderPipeline,

    uniform_bgl: wgpu::BindGroupLayout,
    pub texture_bgl: wgpu::BindGroupLayout,
    blend_bgl: wgpu::BindGroupLayout,

    pub sampler: wgpu::Sampler,

    quad_vbo: wgpu::Buffer,
    quad_ibo: wgpu::Buffer,
}

impl Gpu {
    /// Create the device from a first surface and build all pipelines.
    pub fn new(first_window: Arc<Window>) -> anyhow::Result<(Self, wgpu::Surface<'static>)> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(first_window)?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or_else(|| anyhow::anyhow!("no suitable GPU adapter found"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("lcme-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))?;

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene-uniform-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let blend_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blend-bgl"),
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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear-clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let scene_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/scene.wgsl").into()),
        });
        let warp_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("warp.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/warp.wgsl").into()),
        });

        const VERTEX_ATTRS: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX_ATTRS,
        };

        let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene-layout"),
            bind_group_layouts: &[&uniform_bgl, &texture_bgl],
            push_constant_ranges: &[],
        });
        let scene_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scene-pipeline"),
            layout: Some(&scene_layout),
            vertex: wgpu::VertexState {
                module: &scene_shader,
                entry_point: Some("vs"),
                buffers: std::slice::from_ref(&vertex_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &scene_shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: SCENE_FORMAT,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let warp_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("warp-layout"),
            bind_group_layouts: &[&blend_bgl, &texture_bgl],
            push_constant_ranges: &[],
        });
        let warp_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("warp-pipeline"),
            layout: Some(&warp_layout),
            vertex: wgpu::VertexState {
                module: &warp_shader,
                entry_point: Some("vs"),
                buffers: std::slice::from_ref(&vertex_layout),
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &warp_shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let quad_vbo = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-vbo"),
            contents: bytemuck::cast_slice(&QUAD_VERTS),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let quad_ibo = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-ibo"),
            contents: bytemuck::cast_slice(&QUAD_INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        Ok((
            Gpu {
                instance,
                adapter,
                device,
                queue,
                surface_format,
                scene_pipeline,
                warp_pipeline,
                uniform_bgl,
                texture_bgl,
                blend_bgl,
                sampler,
                quad_vbo,
                quad_ibo,
            },
            surface,
        ))
    }

    /// Create a surface for an additional window (outputs after the first).
    pub fn create_surface(&self, window: Arc<Window>) -> anyhow::Result<wgpu::Surface<'static>> {
        Ok(self.instance.create_surface(window)?)
    }

    /// Build the per-output GPU state once the surface and window size are known.
    pub fn build_output(
        &self,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        output: &Output,
        layer_count: usize,
    ) -> OutputGpu {
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&self.device, &config);

        let (scene_tex, scene_bg) = self.make_scene_target(config.width, config.height);

        // Per-layer uniform buffers + bind groups (values rewritten each frame).
        let mut layer_uniforms = Vec::with_capacity(layer_count);
        let mut layer_uniform_bgs = Vec::with_capacity(layer_count);
        for i in 0..layer_count {
            let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("scene-uniform"),
                size: std::mem::size_of::<SceneUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scene-uniform-bg"),
                layout: &self.uniform_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buf.as_entire_binding(),
                }],
            });
            let _ = i;
            layer_uniforms.push(buf);
            layer_uniform_bgs.push(bg);
        }

        // Warp mesh + blend uniform for this output.
        let (warp_vbo, warp_ibo, warp_index_count) = self.make_warp_mesh(output);
        let blend = BlendUniform {
            left: output.blend.left,
            right: output.blend.right,
            top: output.blend.top,
            bottom: output.blend.bottom,
            gamma: output.blend.gamma,
            _pad: [0.0; 3],
        };
        let blend_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blend-uniform"),
                contents: bytemuck::bytes_of(&blend),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let blend_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blend-bg"),
            layout: &self.blend_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: blend_buf.as_entire_binding(),
            }],
        });

        OutputGpu {
            surface,
            config,
            scene_tex,
            scene_bg,
            layer_uniforms,
            layer_uniform_bgs,
            warp_vbo,
            warp_ibo,
            warp_index_count,
            _blend_buf: blend_buf,
            blend_bg,
        }
    }

    fn make_scene_target(&self, width: u32, height: u32) -> (wgpu::Texture, wgpu::BindGroup) {
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scene-slice"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SCENE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scene-slice-bg"),
            layout: &self.texture_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        (tex, bg)
    }

    /// Build the warp mesh for one output: a grid whose destination positions
    /// come from bilinear interpolation of the four (optionally moved) corners,
    /// then the display rotation, converted to NDC. UVs are the grid params.
    fn make_warp_mesh(&self, output: &Output) -> (wgpu::Buffer, wgpu::Buffer, u32) {
        // Corners in normalized output space (TL, TR, BR, BL).
        let corners =
            output
                .warp
                .corners
                .unwrap_or([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        let tl = glam::Vec2::from(corners[0]);
        let tr = glam::Vec2::from(corners[1]);
        let br = glam::Vec2::from(corners[2]);
        let bl = glam::Vec2::from(corners[3]);

        let theta = (output.rotation_degrees as f32).to_radians();
        let (s, c) = theta.sin_cos();

        let mut verts: Vec<Vertex> =
            Vec::with_capacity(((WARP_GRID + 1) * (WARP_GRID + 1)) as usize);
        for gy in 0..=WARP_GRID {
            let v = gy as f32 / WARP_GRID as f32;
            for gx in 0..=WARP_GRID {
                let u = gx as f32 / WARP_GRID as f32;
                // Bilinear interpolation of the four corners.
                let top = tl.lerp(tr, u);
                let bottom = bl.lerp(br, u);
                let mut p = top.lerp(bottom, v); // normalized output space, y-down

                // Display rotation about the center.
                let d = p - glam::Vec2::splat(0.5);
                p = glam::Vec2::new(0.5 + d.x * c - d.y * s, 0.5 + d.x * s + d.y * c);

                // Normalized (y-down) -> NDC (y-up).
                let ndc = [p.x * 2.0 - 1.0, 1.0 - p.y * 2.0];
                verts.push(Vertex {
                    pos: ndc,
                    uv: [u, v],
                });
            }
        }

        let stride = WARP_GRID + 1;
        let mut indices: Vec<u32> = Vec::with_capacity((WARP_GRID * WARP_GRID * 6) as usize);
        for gy in 0..WARP_GRID {
            for gx in 0..WARP_GRID {
                let i0 = gy * stride + gx;
                let i1 = i0 + 1;
                let i2 = i0 + stride;
                let i3 = i2 + 1;
                indices.extend_from_slice(&[i0, i1, i2, i2, i1, i3]);
            }
        }

        let vbo = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("warp-vbo"),
                contents: bytemuck::cast_slice(&verts),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let ibo = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("warp-ibo"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            });
        (vbo, ibo, indices.len() as u32)
    }

    pub fn resize_output(&self, out: &mut OutputGpu, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        out.config.width = width;
        out.config.height = height;
        out.surface.configure(&self.device, &out.config);
        let (tex, bg) = self.make_scene_target(width, height);
        out.scene_tex = tex;
        out.scene_bg = bg;
    }

    /// Render one output for the current show time.
    pub fn render_output(
        &self,
        out: &mut OutputGpu,
        show: &Show,
        now: f64,
        contents: &mut [Box<dyn Content>],
        draw_order: &[usize],
        canvas_output: &Output,
    ) -> Result<(), wgpu::SurfaceError> {
        let frame = out.surface.get_current_texture()?;
        let frame_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let view_proj = canvas_output.canvas_rect.view_projection();

        // Write per-layer uniforms for this output.
        for &i in draw_order {
            let layer = &show.layers[i];
            if !layer.timing.visible_at(now) {
                continue;
            }
            let mvp = view_proj * layer.transform.model_matrix();
            let u = SceneUniform {
                mvp: mvp.to_cols_array_2d(),
                opacity: layer.transform.opacity,
                _pad: [0.0; 3],
            };
            self.queue
                .write_buffer(&out.layer_uniforms[i], 0, bytemuck::bytes_of(&u));
        }

        let scene_view = out
            .scene_tex
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame-encoder"),
            });

        // --- scene pass ---
        {
            let bg = show.canvas.background;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scene_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg[0] as f64,
                            g: bg[1] as f64,
                            b: bg[2] as f64,
                            a: bg[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.scene_pipeline);
            pass.set_vertex_buffer(0, self.quad_vbo.slice(..));
            pass.set_index_buffer(self.quad_ibo.slice(..), wgpu::IndexFormat::Uint16);
            for &i in draw_order {
                if !show.layers[i].timing.visible_at(now) {
                    continue;
                }
                pass.set_bind_group(0, &out.layer_uniform_bgs[i], &[]);
                pass.set_bind_group(1, contents[i].bind_group(), &[]);
                pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, 0..1);
            }
        }

        // --- warp + blend pass ---
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("warp-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.warp_pipeline);
            pass.set_bind_group(0, &out.blend_bg, &[]);
            pass.set_bind_group(1, &out.scene_bg, &[]);
            pass.set_vertex_buffer(0, out.warp_vbo.slice(..));
            pass.set_index_buffer(out.warp_ibo.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..out.warp_index_count, 0, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// Expose the adapter name for logging.
    pub fn adapter_info(&self) -> String {
        let info = self.adapter.get_info();
        format!("{} ({:?})", info.name, info.backend)
    }
}

/// Per-output GPU state.
pub struct OutputGpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    scene_tex: wgpu::Texture,
    scene_bg: wgpu::BindGroup,
    layer_uniforms: Vec<wgpu::Buffer>,
    layer_uniform_bgs: Vec<wgpu::BindGroup>,
    warp_vbo: wgpu::Buffer,
    warp_ibo: wgpu::Buffer,
    warp_index_count: u32,
    _blend_buf: wgpu::Buffer,
    blend_bg: wgpu::BindGroup,
}
