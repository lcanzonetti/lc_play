//! The GPU compositor.
//!
//! [`Gpu`] owns the wgpu device/queue and the two render pipelines (scene and
//! warp). Each [`OutputGpu`] owns one window's surface, an offscreen "canvas
//! slice" texture, per-layer uniform buffers, and the warp mesh + blend uniform
//! for that output.
//!
//! A frame renders **every output together**: one show-clock sample, one
//! command encoder, one queue submit, then all surfaces are presented. That is
//! what keeps outputs in lockstep — they can never show different show times,
//! and N windows don't serialize on N separate vsync waits.
//!
//! Per output, the frame runs two passes:
//!   1. **scene** → render every visible layer into the slice texture using the
//!      output's view-projection (so content spanning outputs is clipped per
//!      output automatically);
//!   2. **warp** → sample the slice through the perspective warp mesh and
//!      multiply by the edge-blend mask, writing to the window surface.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use glam::{Mat3, Vec2, Vec3};
use wgpu::util::DeviceExt as _;
use winit::window::Window;

use crate::content::{Content, TextureFactory};
use crate::show::{Output, Show};

/// Offscreen "canvas slice" format. sRGB so shader math stays in linear space.
const SCENE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// Warp mesh resolution (cells per side). The corner-pin warp is exact at any
/// resolution (perspective-correct interpolation); the grid exists so freeform
/// mesh warps (Phase 3) can move interior points.
const WARP_GRID: u32 = 16;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct SceneUniform {
    mvp: [[f32; 4]; 4],
    opacity: f32,
    /// How the layer's texture(s) encode its pixels; see `Sampling`.
    mode: u32,
    uv_scale: [f32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct BlendUniform {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    gamma_exp: f32,
    _pad: [f32; 3],
}

// Uniform layouts must match the WGSL structs byte-for-byte.
const _: () = assert!(std::mem::size_of::<SceneUniform>() == 80);
const _: () = assert!(std::mem::size_of::<BlendUniform>() == 32);

/// Unit-quad vertex for the scene pass.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct QuadVertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

/// Warp-mesh vertex: homogeneous clip position + slice UV.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct WarpVertex {
    clip: [f32; 4],
    uv: [f32; 2],
}

const QUAD_VERTS: [QuadVertex; 4] = [
    QuadVertex {
        pos: [0.0, 0.0],
        uv: [0.0, 0.0],
    },
    QuadVertex {
        pos: [1.0, 0.0],
        uv: [1.0, 0.0],
    },
    QuadVertex {
        pos: [0.0, 1.0],
        uv: [0.0, 1.0],
    },
    QuadVertex {
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
        // Try the preferred graphics API first, then any.
        let mut found = None;
        for backends in [preferred_backends(), wgpu::Backends::all()] {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                backends,
                ..Default::default()
            });
            let surface = instance.create_surface(first_window.clone())?;
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                }));
            if let Some(adapter) = adapter {
                found = Some((instance, surface, adapter));
                break;
            }
        }
        let (instance, surface, adapter) =
            found.ok_or_else(|| anyhow::anyhow!("no suitable GPU adapter found"))?;

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let gpu = Self::with_adapter(instance, adapter, surface_format)?;
        Ok((gpu, surface))
    }

    /// A device with no window, for offscreen rendering tests. `None` when the
    /// machine has no usable adapter (e.g. CI without a software rasterizer).
    #[cfg(test)]
    pub fn headless() -> Option<Self> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&Default::default()))?;
        Self::with_adapter(instance, adapter, SCENE_FORMAT).ok()
    }

    /// Create the device and build every pipeline for a given output format.
    fn with_adapter(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        surface_format: wgpu::TextureFormat,
    ) -> anyhow::Result<Self> {
        // Request the adapter's real limits rather than the conservative
        // defaults (8192px max texture): canvas-spanning content such as a
        // 3×4K panorama needs the full size the GPU supports.
        // Block-compressed textures (BC1/BC3/BC7) are what HAP video is made
        // of: with them the GPU decodes HAP itself. Every desktop GPU has
        // them; without, HAP falls back to the (slow) ffmpeg path.
        let features = adapter.features() & wgpu::Features::TEXTURE_COMPRESSION_BC;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("lc-runner-device"),
                required_features: features,
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))?;

        let uniform_entry = |visibility| wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scene-uniform-bgl"),
            entries: &[uniform_entry(wgpu::ShaderStages::VERTEX_FRAGMENT)],
        });
        let blend_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blend-bgl"),
            entries: &[uniform_entry(wgpu::ShaderStages::FRAGMENT)],
        });
        // Up to three texture planes (RGBA uses the first; YUV all three)
        // and a sampler. Single-plane content binds its texture three times.
        let plane = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture-bgl"),
            entries: &[
                plane(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                plane(2),
                plane(3),
            ],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear-clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
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

        const QUAD_ATTRS: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];
        const WARP_ATTRS: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x2];
        let quad_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<QuadVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &QUAD_ATTRS,
        };
        let warp_vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<WarpVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &WARP_ATTRS,
        };

        let pipeline = |label: &str,
                        layouts: &[&wgpu::BindGroupLayout],
                        shader: &wgpu::ShaderModule,
                        vertex_layout: wgpu::VertexBufferLayout,
                        format: wgpu::TextureFormat,
                        blend: Option<wgpu::BlendState>| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: layouts,
                push_constant_ranges: &[],
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs"),
                    buffers: &[vertex_layout],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
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
            })
        };

        // Scene: premultiplied "over" compositing of layers.
        let scene_pipeline = pipeline(
            "scene-pipeline",
            &[&uniform_bgl, &texture_bgl],
            &scene_shader,
            quad_layout,
            SCENE_FORMAT,
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        // Warp: the final image replaces the surface contents; no blending.
        let warp_pipeline = pipeline(
            "warp-pipeline",
            &[&blend_bgl, &texture_bgl],
            &warp_shader,
            warp_vertex_layout,
            surface_format,
            None,
        );

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

        Ok(Gpu {
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
        })
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
        // A surface larger than the GPU's texture limit is a hard error
        // (panic) in wgpu. Sizes come from the window system and, through the
        // show, from the network: clamp rather than trust.
        let max = self.max_texture_dimension();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            width: width.clamp(1, max),
            height: height.clamp(1, max),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&self.device, &config);
        let res = self.build_resources(config.width, config.height, output, layer_count);
        OutputGpu {
            surface,
            config,
            res,
        }
    }

    /// Everything an output needs to render, independent of where the final
    /// image goes (a window surface, or an offscreen texture in tests).
    fn build_resources(
        &self,
        width: u32,
        height: u32,
        output: &Output,
        layer_count: usize,
    ) -> OutputResources {
        // A 90°/270° display shows the slice sideways, so render the slice at
        // the rotated resolution to keep it pixel-for-pixel with the display.
        let rotated = matches!(output.rotation_degrees, 90 | 270);
        let (scene_tex, scene_view, scene_bg) = self.make_scene_target(width, height, rotated);

        let (layer_uniforms, layer_uniform_bgs) = self.make_layer_uniforms(layer_count);

        // Warp mesh + blend uniform for this output.
        let (warp_vbo, warp_ibo, warp_index_count) = self.make_warp_mesh(output);
        let (blend_buf, blend_bg) = self.make_blend(output);

        OutputResources {
            rotated,
            _scene_tex: scene_tex,
            scene_view,
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

    fn make_blend(&self, output: &Output) -> (wgpu::Buffer, wgpu::BindGroup) {
        let blend = BlendUniform {
            left: output.blend.left,
            right: output.blend.right,
            top: output.blend.top,
            bottom: output.blend.bottom,
            gamma_exp: 2.2 / output.blend.gamma,
            _pad: [0.0; 3],
        };
        let buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blend-uniform"),
                contents: bytemuck::bytes_of(&blend),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blend-bg"),
            layout: &self.blend_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buf.as_entire_binding(),
            }],
        });
        (buf, bg)
    }

    /// Apply an edited output's geometry — warp, blend, rotation — to its GPU
    /// state in place. (Which part of the canvas it shows is read every
    /// frame anyway.) Far cheaper, and invisible, compared with closing and
    /// reopening its window.
    pub fn update_output(&self, out: &mut OutputGpu, output: &Output) {
        let res = &mut out.res;
        (res.warp_vbo, res.warp_ibo, res.warp_index_count) = self.make_warp_mesh(output);
        (res._blend_buf, res.blend_bg) = self.make_blend(output);
        let rotated = matches!(output.rotation_degrees, 90 | 270);
        if rotated != res.rotated {
            res.rotated = rotated;
            let (tex, view, bg) =
                self.make_scene_target(out.config.width, out.config.height, rotated);
            res._scene_tex = tex;
            res.scene_view = view;
            res.scene_bg = bg;
        }
    }

    /// Per-layer uniform buffers + bind groups (values rewritten each frame).
    fn make_layer_uniforms(&self, layer_count: usize) -> (Vec<wgpu::Buffer>, Vec<wgpu::BindGroup>) {
        (0..layer_count)
            .map(|_| {
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
                (buf, bg)
            })
            .unzip()
    }

    /// Resize an output's per-layer resources after the show's layer list
    /// changed (live editing): the output itself — surface, warp, blend —
    /// stays as it is.
    pub fn set_layer_count(&self, out: &mut OutputGpu, layer_count: usize) {
        if out.res.layer_uniforms.len() != layer_count {
            let (bufs, bgs) = self.make_layer_uniforms(layer_count);
            out.res.layer_uniforms = bufs;
            out.res.layer_uniform_bgs = bgs;
        }
    }

    fn make_scene_target(
        &self,
        width: u32,
        height: u32,
        rotated: bool,
    ) -> (wgpu::Texture, wgpu::TextureView, wgpu::BindGroup) {
        let (width, height) = if rotated {
            (height, width)
        } else {
            (width, height)
        };
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
            entries: &plane_entries([&view, &view, &view], &self.sampler),
        });
        (tex, view, bg)
    }

    fn make_warp_mesh(&self, output: &Output) -> (wgpu::Buffer, wgpu::Buffer, u32) {
        let (verts, indices) = warp_mesh(output);
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
        let max = self.max_texture_dimension();
        let (width, height) = (width.min(max), height.min(max));
        out.config.width = width;
        out.config.height = height;
        out.surface.configure(&self.device, &out.config);
        let res = &mut out.res;
        let (tex, view, bg) = self.make_scene_target(width, height, res.rotated);
        res._scene_tex = tex;
        res.scene_view = view;
        res.scene_bg = bg;
    }

    /// Acquire every output's surface, encode all of them into one command
    /// buffer, and submit once. `times` gives each layer the time of the
    /// timeline it belongs to (`None` = that timeline is off stage). Call
    /// [`PendingFrame::present`] afterwards to put the frame on screen.
    pub fn draw(
        &self,
        targets: &[(&OutputGpu, &Output)],
        show: &Show,
        times: &[Option<f64>],
        contents: &[Box<dyn Content>],
        draw_order: &[usize],
    ) -> PendingFrame {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame-encoder"),
            });
        let mut frames = Vec::with_capacity(targets.len());
        let mut errors = Vec::new();

        for (ti, &(out, output)) in targets.iter().enumerate() {
            match out.surface.get_current_texture() {
                Ok(frame) => {
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    self.encode_output(
                        &mut encoder,
                        &view,
                        &out.res,
                        output,
                        show,
                        times,
                        contents,
                        draw_order,
                    );
                    frames.push(frame);
                }
                Err(e) => errors.push((ti, e)),
            }
        }

        self.queue.submit(Some(encoder.finish()));
        PendingFrame { frames, errors }
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_output(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_view: &wgpu::TextureView,
        out: &OutputResources,
        output: &Output,
        show: &Show,
        times: &[Option<f64>],
        contents: &[Box<dyn Content>],
        draw_order: &[usize],
    ) {
        let view_proj = output.canvas_rect.view_projection();
        let visible = || {
            draw_order
                .iter()
                .copied()
                // `times[i]` is layer i's own timeline time, `None` while
                // that timeline is off stage.
                .filter(|&i| times[i].is_some_and(|t| show.layers[i].timing.visible_at(t)))
        };

        // Per-layer uniforms for this output (staged; applied at submit).
        for i in visible() {
            let t = &show.layers[i].transform;
            let sampling = contents[i].sampling();
            let u = SceneUniform {
                mvp: (view_proj * t.model_matrix()).to_cols_array_2d(),
                // The cue's own opacity times its fade in/out at this instant.
                opacity: t.opacity * show.layers[i].timing.fade_at(times[i].unwrap_or(0.0)),
                mode: sampling.mode as u32,
                uv_scale: sampling.uv_scale,
            };
            self.queue
                .write_buffer(&out.layer_uniforms[i], 0, bytemuck::bytes_of(&u));
        }

        // --- scene pass ---
        {
            // Clear color is premultiplied like everything else in the slice.
            let [r, g, b, a] = show.canvas.background.map(f64::from);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out.scene_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r * a,
                            g: g * a,
                            b: b * a,
                            a,
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
            for i in visible() {
                pass.set_bind_group(0, &out.layer_uniform_bgs[i], &[]);
                pass.set_bind_group(1, contents[i].bind_group(), &[]);
                pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, 0..1);
            }
        }

        // --- warp + blend pass ---
        self.encode_warp(encoder, frame_view, out);
    }

    /// The final pass of an output: its rendered canvas slice, through the
    /// warp mesh and the blend mask, into `target`.
    fn encode_warp(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        out: &OutputResources,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("warp-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
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

    /// Start capturing a small picture of what `out` shows — after warp and
    /// blend, i.e. what reaches the display — for the operator's preview.
    /// Uses the canvas slice rendered in the last frame. The result is read
    /// with [`Capture::take`] once the GPU has finished.
    pub fn capture_output(&self, out: &OutputGpu, name: &str, max_width: u32) -> Capture {
        let (ow, oh) = (out.config.width.max(1), out.config.height.max(1));
        let width = ow.min(max_width).max(1);
        let height = ((oh as u64 * width as u64 / ow as u64) as u32).max(1);
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
            format: self.surface_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        // Buffer rows must be multiples of 256 bytes.
        let padded = (width * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview-readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("preview"),
            });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.encode_warp(&mut encoder, &view, &out.res);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ready.clone();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                if result.is_ok() {
                    flag.store(true, std::sync::atomic::Ordering::Release);
                }
            });
        Capture {
            name: name.to_string(),
            buffer,
            width,
            height,
            padded,
            bgra: matches!(
                self.surface_format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
            ),
            ready,
        }
    }

    /// Let the GPU run completion callbacks (captures) without waiting.
    pub fn poll(&self) {
        self.device.poll(wgpu::Maintain::Poll);
    }

    /// Expose the adapter name for logging.
    pub fn adapter_info(&self) -> String {
        let info = self.adapter.get_info();
        format!("{} ({:?})", info.name, info.backend)
    }

    /// What content needs to create and fill its textures.
    pub fn factory(&self) -> TextureFactory<'_> {
        TextureFactory {
            device: &self.device,
            queue: &self.queue,
            layout: &self.texture_bgl,
            sampler: &self.sampler,
            max_dimension: self.max_texture_dimension(),
            supports_bc: self.supports_bc(),
        }
    }

    /// Whether the GPU samples block-compressed (BCn) textures — the
    /// precondition for GPU-decoded HAP.
    pub fn supports_bc(&self) -> bool {
        self.device
            .features()
            .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    }

    /// Largest texture edge this device accepts.
    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
}

/// A frame that has been submitted but not yet shown.
pub struct PendingFrame {
    frames: Vec<wgpu::SurfaceTexture>,
    /// Outputs whose surface could not be acquired, by index into `targets`.
    pub errors: Vec<(usize, wgpu::SurfaceError)>,
}

impl PendingFrame {
    /// Present every acquired surface back-to-back.
    pub fn present(self) -> Vec<(usize, wgpu::SurfaceError)> {
        for frame in self.frames {
            frame.present();
        }
        self.errors
    }
}

/// Build one output's warp mesh.
///
/// Each grid point `(u, v)` is pushed through the corner-pin homography, then
/// the display rotation and the normalized→NDC mapping. Those last two are
/// affine, so the whole chain is a single 3×3 matrix and the result stays
/// homogeneous: we emit `(x, y, 0, w)` and let the GPU divide, which makes the
/// interpolated slice UVs perspective-correct.
fn warp_mesh(output: &Output) -> (Vec<WarpVertex>, Vec<u32>) {
    let center = Vec2::splat(0.5);
    let rotate = Mat3::from_translation(center)
        * Mat3::from_angle((output.rotation_degrees as f32).to_radians())
        * Mat3::from_translation(-center);
    // Normalized output space (y down, 0..1) -> NDC (y up, -1..1).
    let to_ndc = Mat3::from_cols(
        Vec3::new(2.0, 0.0, 0.0),
        Vec3::new(0.0, -2.0, 0.0),
        Vec3::new(-1.0, 1.0, 1.0),
    );
    let m = to_ndc * rotate * output.warp.homography();

    let n = WARP_GRID;
    let mut verts = Vec::with_capacity(((n + 1) * (n + 1)) as usize);
    for gy in 0..=n {
        let v = gy as f32 / n as f32;
        for gx in 0..=n {
            let u = gx as f32 / n as f32;
            let p = m * Vec3::new(u, v, 1.0);
            verts.push(WarpVertex {
                clip: [p.x, p.y, 0.0, p.z],
                uv: [u, v],
            });
        }
    }

    let stride = n + 1;
    let mut indices = Vec::with_capacity((n * n * 6) as usize);
    for gy in 0..n {
        for gx in 0..n {
            let i0 = gy * stride + gx;
            let (i1, i2) = (i0 + 1, i0 + stride);
            indices.extend_from_slice(&[i0, i1, i2, i2, i1, i2 + 1]);
        }
    }
    (verts, indices)
}

/// A picture of an output on its way back from the GPU.
pub struct Capture {
    pub name: String,
    buffer: wgpu::Buffer,
    pub width: u32,
    pub height: u32,
    padded: u32,
    bgra: bool,
    ready: Arc<std::sync::atomic::AtomicBool>,
}

impl Capture {
    /// Whether the GPU has delivered the picture.
    pub fn take_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    /// RGBA pixels, once the GPU has delivered them.
    pub fn take(&self) -> Option<Vec<u8>> {
        if !self.ready.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let data = self.buffer.slice(..).get_mapped_range();
        let mut rgba = Vec::with_capacity((self.width * self.height * 4) as usize);
        for row in data.chunks(self.padded as usize) {
            rgba.extend_from_slice(&row[..(self.width * 4) as usize]);
        }
        drop(data);
        self.buffer.unmap();
        if self.bgra {
            for px in rgba.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        Some(rgba)
    }
}

/// The bind-group entries for three texture planes and a sampler, in the
/// order of `texture_bgl`.
pub fn plane_entries<'a>(
    planes: [&'a wgpu::TextureView; 3],
    sampler: &'a wgpu::Sampler,
) -> [wgpu::BindGroupEntry<'a>; 4] {
    [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(planes[0]),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::Sampler(sampler),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: wgpu::BindingResource::TextureView(planes[1]),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: wgpu::BindingResource::TextureView(planes[2]),
        },
    ]
}

/// The graphics APIs tried first. On Windows that is Direct3D 12: it is the
/// native path for presenting to windows there, and copes with laptops whose
/// screen is wired to the integrated GPU while rendering happens on the
/// discrete one (NVIDIA Optimus) better than Vulkan does. Elsewhere: Vulkan
/// or Metal. `LCPLAY_BACKEND` (dx12, vulkan, metal, gl) overrides.
fn preferred_backends() -> wgpu::Backends {
    match std::env::var("LCPLAY_BACKEND")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "dx12" => wgpu::Backends::DX12,
        "vulkan" => wgpu::Backends::VULKAN,
        "metal" => wgpu::Backends::METAL,
        "gl" => wgpu::Backends::GL,
        _ if cfg!(windows) => wgpu::Backends::DX12,
        _ => wgpu::Backends::PRIMARY,
    }
}

/// Per-output GPU state: the window surface plus its render resources.
pub struct OutputGpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    res: OutputResources,
}

struct OutputResources {
    rotated: bool,
    _scene_tex: wgpu::Texture,
    scene_view: wgpu::TextureView,
    scene_bg: wgpu::BindGroup,
    layer_uniforms: Vec<wgpu::Buffer>,
    layer_uniform_bgs: Vec<wgpu::BindGroup>,
    warp_vbo: wgpu::Buffer,
    warp_ibo: wgpu::Buffer,
    warp_index_count: u32,
    _blend_buf: wgpu::Buffer,
    blend_bg: wgpu::BindGroup,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::Show;

    fn output(json_extra: &str) -> Output {
        let json = format!(
            r#"{{ "canvas": {{ "width": 100, "height": 100 }},
                 "outputs": [ {{ "id": "o", "canvas_rect": {{ "x": 0, "y": 0, "w": 100, "h": 100 }} {json_extra} }} ] }}"#
        );
        Show::from_json(&json).unwrap().outputs.remove(0)
    }

    fn ndc(v: &WarpVertex) -> Vec2 {
        Vec2::new(v.clip[0] / v.clip[3], v.clip[1] / v.clip[3])
    }

    #[test]
    fn identity_mesh_covers_the_full_viewport() {
        let (verts, _) = warp_mesh(&output(""));
        let n = WARP_GRID as usize;
        assert!(ndc(&verts[0]).distance(Vec2::new(-1.0, 1.0)) < 1e-5); // TL
        assert!(ndc(&verts[n]).distance(Vec2::new(1.0, 1.0)) < 1e-5); // TR
        assert!(ndc(verts.last().unwrap()).distance(Vec2::new(1.0, -1.0)) < 1e-5);
        // BR
    }

    #[test]
    fn keystone_mesh_has_positive_w_everywhere() {
        // w <= 0 would flip or clip geometry; a valid convex pin must avoid it.
        let (verts, _) = warp_mesh(&output(
            r#", "warp": { "corners": [[0.25,0.0],[0.75,0.0],[1.0,1.0],[0.0,1.0]] }"#,
        ));
        assert!(verts.iter().all(|v| v.clip[3] > 0.0));
    }

    #[test]
    fn rotation_90_moves_top_left_to_top_right() {
        // Rotating the image 90° (clockwise on a y-down display) sends the
        // slice's top-left corner to the display's top-right.
        let (verts, _) = warp_mesh(&output(r#", "rotation_degrees": 90"#));
        assert!(ndc(&verts[0]).distance(Vec2::new(1.0, 1.0)) < 1e-5);
    }

    // ---- offscreen GPU tests -------------------------------------------------
    //
    // These run the real pipelines and shaders through `encode_output` and read
    // the pixels back. They need a GPU adapter; a software one (Mesa lavapipe)
    // is enough. Without any adapter they skip rather than fail.

    use crate::content::{self, srgb_to_linear};

    fn gpu_or_skip() -> Option<Gpu> {
        let gpu = Gpu::headless();
        if gpu.is_none() {
            eprintln!("skipping GPU test: no adapter (install mesa-vulkan-drivers)");
        }
        gpu
    }

    /// Render output `idx` of `show` at t=0 into a `w`×`h` offscreen target and
    /// return its pixels as linear RGB. `w` must be a multiple of 64 (row
    /// alignment for the readback copy).
    fn render_offscreen(gpu: &Gpu, show: &Show, idx: usize, w: u32, h: u32) -> Vec<[f32; 3]> {
        let factory = gpu.factory();
        let contents: Vec<_> = show
            .layers
            .iter()
            .map(|l| content::build(l, &factory).unwrap())
            .collect();
        render_contents(gpu, show, idx, w, h, &contents)
    }

    /// Render one output of `show` with the given (already built) contents.
    fn render_contents(
        gpu: &Gpu,
        show: &Show,
        idx: usize,
        w: u32,
        h: u32,
        contents: &[Box<dyn Content>],
    ) -> Vec<[f32; 3]> {
        let order: Vec<usize> = (0..show.layers.len()).collect();
        let output = &show.outputs[idx];
        let res = gpu.build_resources(w, h, output, show.layers.len());

        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.surface_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test-readback"),
            size: (w * h * 4) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut enc = gpu.device.create_command_encoder(&Default::default());
        let view = target.create_view(&Default::default());
        // Every layer at show time 0, as on a single main timeline.
        let times = vec![Some(0.0); show.layers.len()];
        gpu.encode_output(
            &mut enc, &view, &res, output, show, &times, contents, &order,
        );
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
            },
            size,
        );
        gpu.queue.submit(Some(enc.finish()));

        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        gpu.device.poll(wgpu::Maintain::Wait);
        let bytes = readback.slice(..).get_mapped_range();
        let lin = |b: u8| srgb_to_linear(b as f32 / 255.0);
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| [lin(p[0]), lin(p[1]), lin(p[2])])
            .collect()
    }

    /// A 64×64 canvas with one full-canvas video layer showing `path`.
    fn video_show(path: &str) -> Show {
        let json = serde_json::json!({
            "canvas": { "width": 64, "height": 64 },
            "outputs": [ { "id": "o", "canvas_rect": { "x": 0, "y": 0, "w": 64, "h": 64 } } ],
            "layers": [ { "id": "v", "source": { "type": "video", "path": path },
                          "transform": { "position": [32, 32], "size": [64, 64] } } ]
        });
        Show::from_json(&json.to_string()).unwrap()
    }

    /// Encode a 2 s solid-colour clip with ffmpeg; `None` if that fails.
    fn solid_clip(
        dir: &std::path::Path,
        name: &str,
        colour: &str,
        codec: &[&str],
    ) -> Option<String> {
        let path = dir.join(name);
        let ok = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("color={colour}:s=64x64:r=10:d=2"))
            .args(codec)
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success());
        ok.then(|| path.to_string_lossy().into_owned())
    }

    /// Render until the centre pixel stops being black (content loads and
    /// decodes asynchronously), or give up after a few seconds.
    fn render_when_loaded(gpu: &Gpu, show: &Show) -> [f32; 3] {
        let factory = gpu.factory();
        let mut contents: Vec<_> = show
            .layers
            .iter()
            .map(|l| content::build(l, &factory).unwrap())
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            for c in &mut contents {
                c.update(0.5, &factory);
            }
            let px = render_contents(gpu, show, 0, 64, 64, &contents)[32 * 64 + 32];
            if px.iter().any(|&v| v > 0.01) || std::time::Instant::now() > deadline {
                return px;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn assert_close(got: [f32; 3], want_srgb: [u8; 3], tolerance: f32, what: &str) {
        let want = want_srgb.map(|b| srgb_to_linear(b as f32 / 255.0));
        let ok = got
            .iter()
            .zip(&want)
            .all(|(g, w)| (g - w).abs() <= tolerance);
        assert!(ok, "{what}: got linear {got:?}, expected about {want:?}");
    }

    /// The whole GPU video path, end to end: a file → decoder → textures →
    /// the scene shader's conversion → the right colour on the output.
    #[test]
    fn video_frames_reach_the_output_in_the_right_colours() {
        let Some(gpu) = gpu_or_skip() else { return };
        let dir = std::env::temp_dir().join(format!("lcplay-gpu-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // An orange: different in every channel, mid-range in all.
        let (colour, srgb) = ("0xE08030", [0xE0, 0x80, 0x30]);

        // YUV through ffmpeg (H.264). Video is limited-range 8-bit YUV, so a
        // little rounding is expected.
        let Some(h264) = solid_clip(&dir, "c.mp4", colour, &["-pix_fmt", "yuv420p"]) else {
            eprintln!("SKIPPED: ffmpeg not available");
            return;
        };
        let px = render_when_loaded(&gpu, &video_show(&h264));
        assert_close(px, srgb, 0.03, "H.264 via YUV planes");

        // HAP decoded by the GPU (BC textures), if this GPU samples them.
        if !gpu.supports_bc() {
            eprintln!("SKIPPED (HAP part): no BC texture support on this adapter");
            return;
        }
        for (format, tolerance) in [("hap", 0.04), ("hap_alpha", 0.04), ("hap_q", 0.04)] {
            let name = format!("{format}.mov");
            let codec = ["-c:v", "hap", "-format", format];
            let Some(path) = solid_clip(&dir, &name, colour, &codec) else {
                eprintln!("SKIPPED (HAP part): no HAP encoder");
                return;
            };
            let px = render_when_loaded(&gpu, &video_show(&path));
            assert_close(px, srgb, tolerance, format);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edge_blend_overlap_sums_to_full_light() {
        let Some(gpu) = gpu_or_skip() else { return };
        // Two 128px outputs whose canvas slices overlap by 32px (25%).
        let show = Show::from_json(
            r#"{ "canvas": { "width": 224, "height": 64 },
                 "outputs": [
                   { "id": "a", "canvas_rect": { "x": 0,  "y": 0, "w": 128, "h": 64 }, "blend": { "right": 0.25 } },
                   { "id": "b", "canvas_rect": { "x": 96, "y": 0, "w": 128, "h": 64 }, "blend": { "left": 0.25 } } ],
                 "layers": [ { "id": "white", "source": { "type": "color", "rgba": [1,1,1,1] },
                               "transform": { "position": [112,32], "size": [224,64] } } ] }"#,
        )
        .unwrap();
        let a = render_offscreen(&gpu, &show, 0, 128, 64);
        let b = render_offscreen(&gpu, &show, 1, 128, 64);
        let row = 32 * 128;

        assert!(
            a[row + 10][0] > 0.99,
            "left output dimmed outside the blend"
        );
        assert!(
            b[row + 100][0] > 0.99,
            "right output dimmed outside the blend"
        );
        // Canvas x 96..128 is seen by both: the light must add up to 1.
        for j in 0..32 {
            let (la, lb) = (a[row + 96 + j][0], b[row + j][0]);
            assert!(
                (la + lb - 1.0).abs() < 0.02,
                "canvas x {}: {la:.3} + {lb:.3} != 1",
                96 + j
            );
        }
        // A real ramp, not a hard cut.
        assert!((0.2..0.8).contains(&a[row + 112][0]));
    }

    #[test]
    fn translucent_layer_composites_in_linear_light() {
        let Some(gpu) = gpu_or_skip() else { return };
        let show = Show::from_json(
            r#"{ "canvas": { "width": 64, "height": 64 },
                 "outputs": [ { "id": "o", "canvas_rect": { "x": 0, "y": 0, "w": 64, "h": 64 } } ],
                 "layers": [ { "id": "red", "source": { "type": "color", "rgba": [1,0,0,0.5] },
                               "transform": { "position": [32,32], "size": [64,64] } } ] }"#,
        )
        .unwrap();
        let px = render_offscreen(&gpu, &show, 0, 64, 64)[32 * 64 + 32];
        assert!((px[0] - 0.5).abs() < 0.01, "red over black: {px:?}");
        assert!(px[1] < 0.01 && px[2] < 0.01, "{px:?}");
    }

    #[test]
    fn corner_pin_confines_the_image_to_the_quad() {
        let Some(gpu) = gpu_or_skip() else { return };
        let show = Show::from_json(
            r#"{ "canvas": { "width": 64, "height": 64 },
                 "outputs": [ { "id": "o", "canvas_rect": { "x": 0, "y": 0, "w": 64, "h": 64 },
                                "warp": { "corners": [[0.25,0.25],[0.75,0.25],[0.75,0.75],[0.25,0.75]] } } ],
                 "layers": [ { "id": "white", "source": { "type": "color", "rgba": [1,1,1,1] },
                               "transform": { "position": [32,32], "size": [64,64] } } ] }"#,
        )
        .unwrap();
        let px = render_offscreen(&gpu, &show, 0, 64, 64);
        let at = |x: usize, y: usize| px[y * 64 + x][0];
        assert!(at(32, 32) > 0.99, "inside the pinned quad should be lit");
        assert!(at(4, 4) < 0.01 && at(10, 32) < 0.01 && at(60, 60) < 0.01);
    }
}
