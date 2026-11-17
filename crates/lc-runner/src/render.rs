//! The GPU compositor.
//!
//! [`Gpu`] owns the wgpu device/queue and the two render pipelines (scene and
//! warp). Each [`OutputGpu`] owns one window's surface, an offscreen "canvas
//! slice" texture, per-layer uniform buffers, and the warp mesh + blend uniform
//! for that output.
//!
//! A Runner has one [`Gpu`] per graphics card that drives an output, so every
//! output is rendered by the card it is connected to (no copy from one card
//! to another by Windows, and each output can join the card's present
//! barrier). On one card, a frame renders **every output together**: one
//! show-clock sample, one command encoder, one queue submit, then every output
//! is presented.
//!
//! A frame is made in three steps, so that the content can be chosen for the
//! instant the picture will be *seen*:
//!   1. [`Gpu::acquire`] each output — this is where the loop waits for the
//!      display; afterwards the refresh the frame is for is known
//!      ([`OutputTiming`]);
//!   2. the caller samples the show clock for that refresh and updates the
//!      contents, then [`Gpu::render`] encodes and submits;
//!   3. [`Gpu::present`] each output.
//!
//! Outputs are presented through a [`Presenter`]: on Windows/D3D12, LC Play's
//! own DXGI swap chain (frame statistics, NVIDIA present barrier — see
//! `dxgi.rs`); elsewhere, or if that fails, wgpu's surface.
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

use crate::content::{Content, TextureFactory, Uploads};
use crate::show::{Output, Show};
use crate::vsync::{PresentTracker, RefreshClock};

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
    /// Extra values for shader-drawn content (`Sampling::param`).
    param: [f32; 4],
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
const _: () = assert!(std::mem::size_of::<SceneUniform>() == 96);
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

    /// Copies recorded by contents for the next frame (see [`Uploads`]).
    uploads: std::cell::RefCell<Uploads>,
}

impl Gpu {
    /// Create the device for the card that should render `window` and build
    /// all pipelines. With `luid` (Windows/D3D12), the card with that adapter
    /// LUID; otherwise the high-performance card that can present to it.
    pub fn new(first_window: Arc<Window>, luid: Option<i64>) -> anyhow::Result<Self> {
        #[cfg(windows)]
        if let Some((instance, adapter)) = luid.and_then(dedicated_adapter) {
            return Self::with_adapter(instance, adapter, wgpu::TextureFormat::Bgra8UnormSrgb);
        }
        let _ = luid;
        let (gpu, _surface) = Self::new_any(first_window)?;
        Ok(gpu)
    }

    /// The device of the high-performance card that can present to `window`.
    fn new_any(first_window: Arc<Window>) -> anyhow::Result<(Self, wgpu::Surface<'static>)> {
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
            uploads: Default::default(),
        })
    }

    /// The copies contents recorded for this frame (texture uploads), to be
    /// submitted ahead of it.
    pub fn take_uploads(&self) -> Option<wgpu::CommandBuffer> {
        self.uploads.borrow_mut().take()
    }

    /// Create a surface for an additional window (outputs after the first).
    /// Whether this device is D3D12 (where LC Play's own swap chain works).
    pub fn is_dx12(&self) -> bool {
        self.adapter.get_info().backend == wgpu::Backend::Dx12
    }

    /// The adapter's LUID (Windows/D3D12), to match it with monitors.
    pub fn luid(&self) -> Option<i64> {
        #[cfg(windows)]
        return crate::dxgi::adapter_luid(&self.adapter);
        #[cfg(not(windows))]
        None
    }

    /// Whether the card is an integrated GPU.
    pub fn is_integrated(&self) -> bool {
        self.adapter.get_info().device_type == wgpu::DeviceType::IntegratedGpu
    }

    /// Build the per-output GPU state for `window`: its presenter and render
    /// resources.
    pub fn build_output(
        &self,
        window: Arc<Window>,
        output: &Output,
        layer_count: usize,
    ) -> anyhow::Result<OutputGpu> {
        // A surface larger than the GPU's texture limit is a hard error
        // (panic) in wgpu. Sizes come from the window system and, through the
        // show, from the network: clamp rather than trust.
        let max = self.max_texture_dimension();
        let phys = window.inner_size();
        let (width, height) = (phys.width.clamp(1, max), phys.height.clamp(1, max));
        let presenter = self.make_presenter(window.clone(), width, height)?;
        let res = self.build_resources(width, height, output, layer_count);
        Ok(OutputGpu {
            window,
            presenter,
            width,
            height,
            res,
            timing: OutputTiming::default(),
        })
    }

    fn make_presenter(
        &self,
        window: Arc<Window>,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Presenter> {
        #[cfg(windows)]
        if self.is_dx12() && crate::dxgi::enabled() {
            let made = crate::dxgi::hwnd_of(&window)
                .ok_or_else(|| anyhow::anyhow!("no window handle"))
                .and_then(|hwnd| {
                    crate::dxgi::SwapChain::new(
                        &self.device,
                        hwnd,
                        width,
                        height,
                        self.surface_format,
                    )
                });
            match made {
                Ok(sc) => return Ok(Presenter::Dxgi(Box::new(sc))),
                Err(e) => tracing::warn!(
                    "own swap chain unavailable ({e:#}); presenting through wgpu, \
                     without frame statistics or present barrier"
                ),
            }
        }
        let surface = self.instance.create_surface(window)?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: frame_latency(),
        };
        surface.configure(&self.device, &config);
        Ok(Presenter::Surface { surface, config })
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
            let (tex, view, bg) = self.make_scene_target(out.width, out.height, rotated);
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
        match &mut out.presenter {
            Presenter::Surface { surface, config } => {
                config.width = width;
                config.height = height;
                surface.configure(&self.device, config);
            }
            #[cfg(windows)]
            Presenter::Dxgi(sc) => {
                let resized = if sc.is_broken() {
                    Err(anyhow::anyhow!("its buffers were lost"))
                } else {
                    sc.resize(&self.device, width, height)
                };
                if let Err(e) = resized {
                    // Start over with a new presenter for the window: the old
                    // swap chain must be gone first (one per window).
                    tracing::error!("output swap chain unusable ({e:#}); recreating it");
                    out.presenter = Presenter::Dead;
                    let _ = self.device.poll(wgpu::Maintain::Wait);
                    out.presenter = self
                        .make_presenter(out.window.clone(), width, height)
                        .unwrap_or_else(|e| {
                            tracing::error!("cannot present to this output any more: {e:#}");
                            Presenter::Dead
                        });
                }
            }
            Presenter::Dead => {}
        }
        out.width = width;
        out.height = height;
        // A new size means a new mode: the refresh train is measured afresh.
        out.timing.clock = RefreshClock::default();
        let res = &mut out.res;
        let (tex, view, bg) = self.make_scene_target(width, height, res.rotated);
        res._scene_tex = tex;
        res.scene_view = view;
        res.scene_bg = bg;
    }

    /// Wait until `out` can take a frame and get the texture to render into.
    /// For the own swap chain, also feeds the driver's refresh statistics
    /// into the output's timing; for a wgpu surface, the moment of release.
    pub fn acquire(
        &self,
        out: &mut OutputGpu,
        local_now: impl Fn() -> i64,
    ) -> Result<Acquired, wgpu::SurfaceError> {
        match &mut out.presenter {
            Presenter::Surface { surface, .. } => {
                let frame = surface.get_current_texture()?;
                out.timing.clock.observe_tick(local_now());
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                Ok(Acquired {
                    view,
                    surface: Some(frame),
                })
            }
            Presenter::Dead => Err(wgpu::SurfaceError::Timeout),
            #[cfg(windows)]
            Presenter::Dxgi(sc) => {
                // 100 ms is far beyond any refresh: the window is hidden or
                // the display gone. Skip the output; and for the next frames
                // only look (no wait), so the other outputs of the card are
                // not held up by it.
                let timeout = if out.timing.stalled > 0 {
                    out.timing.stalled -= 1;
                    0
                } else {
                    100
                };
                if !sc.wait(timeout) {
                    if timeout > 0 {
                        out.timing.stalled = 60;
                    }
                    return Err(wgpu::SurfaceError::Timeout);
                }
                out.timing.stalled = 0;
                let now = local_now();
                match sc.statistics() {
                    Some(st) => {
                        let at = crate::dxgi::qpc_to_local(st.sync_qpc, now);
                        out.timing.clock.observe_refresh(st.sync_refresh, at);
                        out.timing
                            .tracker
                            .landed(st.present_count, st.present_refresh);
                    }
                    // No statistics (composed window, mode change): the
                    // loop's own timing is the best there is.
                    None => out.timing.clock.observe_tick(now),
                }
                out.timing.barrier = sc.barrier_state().as_str();
                match sc.begin() {
                    Some(view) => Ok(Acquired {
                        view: view.clone(),
                        surface: None,
                    }),
                    // Buffers lost: the caller resizes, which replaces it.
                    None => Err(wgpu::SurfaceError::Lost),
                }
            }
        }
    }

    /// Encode every acquired output and submit once. `times` gives each layer
    /// the time of the timeline it belongs to (`None` = that timeline is off
    /// stage).
    pub fn render(
        &self,
        targets: &[(&OutputGpu, &Output, &wgpu::TextureView)],
        show: &Show,
        times: &[Option<f64>],
        contents: &[Box<dyn Content>],
        draw_order: &[usize],
    ) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame-encoder"),
            });
        for &(out, output, view) in targets {
            self.encode_output(
                &mut encoder,
                view,
                &out.res,
                output,
                show,
                times,
                contents,
                draw_order,
            );
        }
        // The frame's texture uploads first, in the same submission.
        self.queue.submit(
            self.take_uploads()
                .into_iter()
                .chain(Some(encoder.finish())),
        );
    }

    /// Put a rendered frame on screen. `target` is the refresh it was
    /// predicted to reach without correction, and the correction applied
    /// (see [`PresentTracker`]).
    pub fn present(&self, out: &mut OutputGpu, frame: Acquired, target: Option<(i64, i64)>) {
        match &mut out.presenter {
            Presenter::Dead => {}
            Presenter::Surface { .. } => {
                if let Some(f) = frame.surface {
                    f.present();
                }
                out.timing.tracker.presents += 1;
            }
            #[cfg(windows)]
            Presenter::Dxgi(sc) => match sc.end() {
                Ok(id) => {
                    let (base, correction) = target.unwrap_or((0, 0));
                    out.timing.tracker.presented(id, base, correction);
                    // Diagnosis only: twice a second at 60 Hz is plenty.
                    if out.timing.tracker.presents % 30 == 1 {
                        out.timing.frame_counter = sc.frame_counter();
                    }
                }
                Err(e) => tracing::warn!("present failed: {e:#}"),
            },
        }
        let _ = target;
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
                // A display's test pattern is for that display only.
                .filter(|&i| {
                    show.layers[i]
                        .only_output
                        .as_ref()
                        .is_none_or(|id| *id == output.id)
                })
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
                param: sampling.param,
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
        let (ow, oh) = (out.width.max(1), out.height.max(1));
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
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ok_flag = ok.clone();
        let flag = ready.clone();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                // Done either way; only a successful map can be read.
                ok_flag.store(result.is_ok(), std::sync::atomic::Ordering::Release);
                flag.store(true, std::sync::atomic::Ordering::Release);
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
            ok,
            started: std::time::Instant::now(),
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
            uploads: &self.uploads,
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
    /// The GPU has finished with it (successfully or not)…
    ready: Arc<std::sync::atomic::AtomicBool>,
    /// …and it can be read.
    ok: Arc<std::sync::atomic::AtomicBool>,
    pub started: std::time::Instant,
}

impl Capture {
    /// Whether the GPU has delivered the picture.
    pub fn take_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    /// RGBA pixels, once the GPU has delivered them.
    pub fn take(&self) -> Option<Vec<u8>> {
        if !self.ready.load(std::sync::atomic::Ordering::Acquire)
            || !self.ok.load(std::sync::atomic::Ordering::Acquire)
        {
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

/// The D3D12 adapter with this LUID, if outputs should be rendered on it:
/// only when D3D12 is the backend in use (`LCPLAY_BACKEND`), and not for an
/// integrated GPU — a laptop's built-in screen hangs off it, and rendering
/// stays on the discrete card as before.
#[cfg(windows)]
pub fn dedicated_adapter(luid: i64) -> Option<(wgpu::Instance, wgpu::Adapter)> {
    if !preferred_backends().contains(wgpu::Backends::DX12) {
        return None;
    }
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..Default::default()
    });
    let adapter = instance
        .enumerate_adapters(wgpu::Backends::DX12)
        .into_iter()
        .find(|a| crate::dxgi::adapter_luid(a) == Some(luid))
        .filter(|a| a.get_info().device_type != wgpu::DeviceType::IntegratedGpu)?;
    Some((instance, adapter))
}

/// Frames rendered ahead of the display with a wgpu surface (1..=3,
/// `LCPLAY_FRAME_LATENCY`, default 2). The own swap chain reads the same.
pub fn frame_latency() -> u32 {
    std::env::var("LCPLAY_FRAME_LATENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2u32)
        .clamp(1, 3)
}

/// How an output's frames reach its display.
pub enum Presenter {
    /// wgpu's surface (every platform).
    Surface {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    },
    /// LC Play's own DXGI swap chain (Windows/D3D12).
    #[cfg(windows)]
    Dxgi(Box<crate::dxgi::SwapChain>),
    /// Nothing works for this window any more (logged); it is skipped.
    Dead,
}

/// A frame being made for one output.
pub struct Acquired {
    pub view: wgpu::TextureView,
    surface: Option<wgpu::SurfaceTexture>,
}

/// One output's presentation timing and health.
#[derive(Default)]
pub struct OutputTiming {
    /// The display's refresh train.
    pub clock: RefreshClock,
    /// Where presents really land (own swap chain only).
    pub tracker: PresentTracker,
    /// Frames where a layer on this output showed an older video frame than
    /// the one due.
    pub late_decodes: u64,
    /// Distance from the Director's refresh grid, ns (`None`: not on it).
    pub grid_offset: Option<i64>,
    /// Present barrier state (`off` for wgpu surfaces).
    pub barrier: &'static str,
    pub frame_counter: Option<u32>,
    /// Frames left during which a stalled output is only polled, not waited
    /// for (see [`Gpu::acquire`]).
    pub stalled: u32,
}

/// Per-output GPU state: how it is presented plus its render resources.
pub struct OutputGpu {
    /// Declared before the window handle's owner drops it: the presenter
    /// must go before the window.
    presenter: Presenter,
    /// To make a new presenter if the swap chain is lost (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    window: Arc<Window>,
    width: u32,
    height: u32,
    res: OutputResources,
    pub timing: OutputTiming,
}

impl OutputGpu {
    /// `dxgi` or `wgpu`, for the health report.
    pub fn presenter_name(&self) -> &'static str {
        match self.presenter {
            Presenter::Surface { .. } => "wgpu",
            #[cfg(windows)]
            Presenter::Dxgi(_) => "dxgi",
            Presenter::Dead => "none",
        }
    }

    /// Whether the refresh numbers are the driver's (and presents are
    /// tracked to the refresh they reach).
    pub fn exact_timing(&self) -> bool {
        self.timing.clock.is_exact()
    }
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
    use crate::testpattern::SweepContent;

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
        gpu.queue
            .submit(gpu.take_uploads().into_iter().chain(Some(enc.finish())));

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

    /// HAP playback through the upload buffers: 40 frames (five times the
    /// ring) at a width whose block rows need padding, each one reaching the
    /// output in turn, and the picture changing exactly when the clip does.
    #[test]
    fn hap_playback_cycles_its_upload_buffers() {
        let Some(gpu) = gpu_or_skip() else { return };
        if !gpu.supports_bc() {
            eprintln!("SKIPPED: no BC texture support on this adapter");
            return;
        }
        let dir = std::env::temp_dir().join(format!("lcplay-gpu-ring-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for format in ["hap", "hap_q"] {
            // 2 s red, then 2 s blue, at 10 fps; 68 px wide: 17 blocks.
            let path = dir.join(format!("{format}.mov"));
            let made = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y"])
                .args(["-f", "lavfi", "-i", "color=red:s=68x52:r=10:d=2"])
                .args(["-f", "lavfi", "-i", "color=blue:s=68x52:r=10:d=2"])
                .args(["-filter_complex", "[0][1]concat=n=2:v=1"])
                .args(["-c:v", "hap", "-format", format])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if !made {
                eprintln!("SKIPPED: no ffmpeg with a HAP encoder");
                return;
            }
            let show = video_show(&path.to_string_lossy());
            let factory = gpu.factory();
            let mut contents = vec![content::build(&show.layers[0], &factory).unwrap()];
            for frame in 0..40 {
                let t = frame as f64 / 10.0 + 0.05;
                // Render until this frame is the one in the texture (it is
                // decoded in the background).
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let px = loop {
                    contents[0].update(t, &factory);
                    let px = render_contents(&gpu, &show, 0, 64, 64, &contents)[32 * 64 + 32];
                    if !contents[0].late() && contents[0].ready() {
                        break px;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "{format}: frame {frame} never arrived"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                };
                let (want, name) = if frame < 20 {
                    ([0xFF, 0, 0], "red")
                } else {
                    ([0, 0, 0xFF], "blue")
                };
                assert_close(px, want, 0.04, &format!("{format} frame {frame} ({name})"));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Render-thread time to bring a new HAP frame into its texture (the
    /// `update` that delivers it), on a file given in `LCPLAY_BENCH_HAP`:
    /// `LCPLAY_BENCH_HAP=big.mov cargo test -p lc-runner --release -- --ignored --nocapture bench_hap_update`
    #[test]
    #[ignore]
    fn bench_hap_update() {
        let Ok(path) = std::env::var("LCPLAY_BENCH_HAP") else {
            return;
        };
        let Some(gpu) = gpu_or_skip() else { return };
        let show = video_show(&path);
        let factory = gpu.factory();
        let mut c = content::build(&show.layers[0], &factory).unwrap();
        let mut times = Vec::new();
        // Every update, including the one that starts the layer.
        let mut slowest = std::time::Duration::ZERO;
        for frame in 0..120 {
            let t = frame as f64 / 25.0 + 0.001;
            loop {
                let start = std::time::Instant::now();
                c.update(t, &factory);
                let took = start.elapsed();
                slowest = slowest.max(took);
                gpu.queue.submit(gpu.take_uploads());
                if c.ready() && !c.late() {
                    times.push(took.as_secs_f64() * 1e3);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        times.sort_by(f64::total_cmp);
        let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
        println!(
            "update delivering a frame: median {:.3} ms, p90 {:.3} ms, max {:.3} ms; \
             slowest update of all (start included) {:.3} ms",
            at(0.5),
            at(0.9),
            at(1.0),
            slowest.as_secs_f64() * 1e3
        );
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
    fn the_sweep_test_pattern_draws_its_lines_where_the_frame_says() {
        let Some(gpu) = gpu_or_skip() else { return };
        let json = |only: &str| {
            format!(
                r#"{{ "canvas": {{ "width": 64, "height": 64 }},
                 "outputs": [ {{ "id": "o", "canvas_rect": {{ "x": 0, "y": 0, "w": 64, "h": 64 }} }} ],
                 "layers": [ {{ "id": "t", "source": {{ "type": "sweep_test", "fps": 50 }},
                               "transform": {{ "position": [32,32], "size": [64,64] }},
                               "only_output": "{only}", "cluster_clock": true }} ] }}"#
            )
        };
        let show = Show::from_json(&json("o")).unwrap();
        let factory = gpu.factory();
        let mut content = content::build(&show.layers[0], &factory).unwrap();
        // Frame 50 at 50 fps: the vertical line half way across (100 steps
        // in 2 s), the horizontal one a third of the way down (150 in 3 s).
        content.update(1.0, &factory);
        let px = render_contents(&gpu, &show, 0, 64, 64, &[content]);
        let (x, y) = SweepContent::positions(50, 50.0);
        let col = (x * 64.0) as usize;
        let row = (y * 64.0) as usize;
        assert!(px[5 * 64 + col][0] > 0.9, "vertical line at column {col}");
        assert!(px[row * 64 + 5][0] > 0.9, "horizontal line at row {row}");
        assert!(px[50 * 64 + 50][0] < 0.01, "transparent elsewhere");

        // A pattern for another display is not drawn on this one.
        let show = Show::from_json(&json("elsewhere")).unwrap();
        let mut content = content::build(&show.layers[0], &factory).unwrap();
        content.update(1.0, &factory);
        let px = render_contents(&gpu, &show, 0, 64, 64, &[content]);
        assert!(px[5 * 64 + col][0] < 0.01, "not on this output");
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
