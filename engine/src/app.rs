//! The windowing + event-loop layer (winit 0.30 `ApplicationHandler`).
//!
//! On start it creates one borderless window per output, brings up the GPU,
//! loads each layer's content once, then drives a continuous frame loop. All
//! windows read the same [`Clock`], so playback stays synchronized across
//! outputs. Keyboard transport: Space = play/pause, ←/→ = seek ±5s,
//! ↓/↑ = seek ±30s, Esc = quit.

use std::collections::HashMap;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::clock::Clock;
use crate::content::{self, Content, TextureFactory};
use crate::render::{Gpu, OutputGpu};
use crate::show::Show;

struct RenderWindow {
    window: Arc<Window>,
    gpu: OutputGpu,
    /// Index into `show.outputs`.
    output_idx: usize,
}

pub struct App {
    show: Show,
    clock: Clock,
    gpu: Option<Gpu>,
    contents: Vec<Box<dyn Content>>,
    windows: Vec<RenderWindow>,
    by_id: HashMap<WindowId, usize>,
    /// Layer indices sorted back-to-front by z.
    draw_order: Vec<usize>,
}

impl App {
    pub fn new(show: Show) -> Self {
        // Only wrap the clock when looping is explicitly requested.
        let loop_len = show
            .playback
            .looping
            .then_some(show.playback.duration)
            .flatten();
        let clock = Clock::new(show.playback.autoplay, loop_len);
        let mut draw_order: Vec<usize> = (0..show.layers.len()).collect();
        draw_order.sort_by(|&a, &b| {
            show.layers[a]
                .transform
                .z
                .partial_cmp(&show.layers[b].transform.z)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        App {
            show,
            clock,
            gpu: None,
            contents: Vec::new(),
            windows: Vec::new(),
            by_id: HashMap::new(),
            draw_order,
        }
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let layer_count = self.show.layers.len();

        // 1. Create one window per output.
        let mut created: Vec<Arc<Window>> = Vec::with_capacity(self.show.outputs.len());
        for output in &self.show.outputs {
            let attrs = window_attributes(output, event_loop);
            created.push(Arc::new(event_loop.create_window(attrs)?));
        }

        // 2. Bring up the GPU on the first window, then load content once.
        let (gpu, first_surface) = Gpu::new(created[0].clone())?;
        tracing::info!("GPU: {}", gpu.adapter_info());
        self.gpu = Some(gpu);
        self.build_contents()?;

        // 3. Build per-output GPU state.
        let gpu = self.gpu.as_ref().unwrap();
        let mut first_surface = Some(first_surface);
        for (idx, window) in created.into_iter().enumerate() {
            let surface = match first_surface.take() {
                Some(s) => s, // reuse the surface created with the device
                None => gpu.create_surface(window.clone())?,
            };
            let phys = window.inner_size();
            let out_gpu = gpu.build_output(
                surface,
                phys.width,
                phys.height,
                &self.show.outputs[idx],
                layer_count,
            );

            self.by_id.insert(window.id(), self.windows.len());
            self.windows.push(RenderWindow {
                window,
                gpu: out_gpu,
                output_idx: idx,
            });
        }

        tracing::info!(
            outputs = self.windows.len(),
            layers = layer_count,
            "show initialized"
        );
        Ok(())
    }

    fn build_contents(&mut self) -> anyhow::Result<()> {
        let gpu = self.gpu.as_ref().unwrap();
        let factory = TextureFactory {
            device: &gpu.device,
            queue: &gpu.queue,
            layout: &gpu.texture_bgl,
            sampler: &gpu.sampler,
        };
        self.contents = self
            .show
            .layers
            .iter()
            .map(|layer| {
                tracing::debug!(layer = %layer.id, "loading content");
                content::build(&layer.source, &factory)
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(())
    }

    fn render(&mut self, idx: usize) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let now = self.clock.now();
        let rw = &mut self.windows[idx];
        let output = &self.show.outputs[rw.output_idx];

        match gpu.render_output(
            &mut rw.gpu,
            &self.show,
            now,
            &mut self.contents,
            &self.draw_order,
            output,
        ) {
            Ok(()) => {}
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                let size = rw.window.inner_size();
                gpu.resize_output(&mut rw.gpu, size.width, size.height);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                tracing::error!("GPU out of memory; exiting");
                std::process::exit(1);
            }
            Err(e) => tracing::warn!("surface error: {e:?}"),
        }
    }
}

/// Build the winit window attributes for one output.
fn window_attributes(
    output: &crate::show::Output,
    event_loop: &ActiveEventLoop,
) -> winit::window::WindowAttributes {
    // Window geometry: explicit window size, else the canvas-rect size.
    let size = output.window.size.unwrap_or([
        output.canvas_rect.w.max(1.0) as u32,
        output.canvas_rect.h.max(1.0) as u32,
    ]);
    let mut attrs = Window::default_attributes()
        .with_title(format!("lcme · {}", output.id))
        .with_inner_size(PhysicalSize::new(size[0], size[1]));

    if let Some([x, y]) = output.window.position {
        attrs = attrs.with_position(PhysicalPosition::new(x, y));
    }
    if output.window.fullscreen {
        let monitor = output
            .window
            .monitor
            .and_then(|i| event_loop.available_monitors().nth(i))
            .or_else(|| event_loop.primary_monitor());
        attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(monitor)));
    }
    attrs
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return; // already initialized
        }
        if let Err(e) = self.initialize(event_loop) {
            tracing::error!("initialization failed: {e:#}");
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(&idx) = self.by_id.get(&window_id) else {
            return;
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_ref() {
                    gpu.resize_output(&mut self.windows[idx].gpu, size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::Space) => self.clock.toggle(),
                    Key::Named(NamedKey::ArrowRight) => self.clock.seek_by(5.0),
                    Key::Named(NamedKey::ArrowLeft) => self.clock.seek_by(-5.0),
                    Key::Named(NamedKey::ArrowUp) => self.clock.seek_by(30.0),
                    Key::Named(NamedKey::ArrowDown) => self.clock.seek_by(-30.0),
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => self.render(idx),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Continuous playback: advance dynamic content once per cycle, then ask
        // every window to redraw. They all read the same clock, so they stay
        // in sync.
        if let Some(gpu) = self.gpu.as_ref() {
            let now = self.clock.now();
            for c in &mut self.contents {
                c.update(now, &gpu.queue);
            }
            for rw in &self.windows {
                rw.window.request_redraw();
            }
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    }
}
