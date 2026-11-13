//! The windowing + event-loop layer (winit 0.30 `ApplicationHandler`).
//!
//! On start it creates one borderless window per output, brings up the GPU,
//! loads each layer's content once, then drives a continuous frame loop. Each
//! frame samples the [`Clock`] **once** and renders every output from that
//! single timestamp in one GPU submission, so outputs never disagree about
//! show time. Pacing comes from vsync (FIFO present).
//!
//! Keyboard transport (only when the show is not `locked`): Space =
//! play/pause, ←/→ = seek ±5s, ↓/↑ = seek ±30s, Esc twice within
//! [`QUIT_CONFIRM_WINDOW`] = quit. A locked show ignores keys and window-close
//! requests entirely.
//!
//! Startup order matters for a show: windows, GPU and all content are loaded
//! first, and the clock starts (with `autoplay`) only after the first frame
//! has been presented, so the opening of the show is never skipped.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// A second Esc must follow the first within this window to quit.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_millis(1500);

pub struct App {
    show: Show,
    clock: Clock,
    /// `playback.autoplay` not yet honoured: the clock starts on the first
    /// presented frame, not at construction.
    autoplay_pending: bool,
    /// Esc must be pressed twice to quit.
    quit_guard: QuitGuard,
    /// Why the engine stopped, if it stopped because of a failure. `main`
    /// returns it, so the process exits non-zero and a supervisor can react.
    fatal: Option<anyhow::Error>,
    gpu: Option<Gpu>,
    contents: Vec<Box<dyn Content>>,
    windows: Vec<RenderWindow>,
    by_id: HashMap<WindowId, usize>,
    /// Layer indices sorted back-to-front by z.
    draw_order: Vec<usize>,
    stats: FrameStats,
}

/// Two-press quit: the first press arms it, a second press within
/// [`QUIT_CONFIRM_WINDOW`] confirms. A single stray Esc does nothing.
#[derive(Default)]
struct QuitGuard {
    armed_at: Option<Instant>,
}

impl QuitGuard {
    /// Register a press at `now`; `true` means "quit now".
    fn press(&mut self, now: Instant) -> bool {
        let confirmed = self
            .armed_at
            .is_some_and(|t| now.saturating_duration_since(t) <= QUIT_CONFIRM_WINDOW);
        // A confirmed press is consumed; otherwise this press arms the guard.
        self.armed_at = if confirmed { None } else { Some(now) };
        confirmed
    }
}

/// Logs the frame rate every few seconds so a show's performance is visible.
struct FrameStats {
    frames: u32,
    since: Instant,
}

impl FrameStats {
    const PERIOD: Duration = Duration::from_secs(5);

    fn tick(&mut self, show_time: f64) {
        self.frames += 1;
        let elapsed = self.since.elapsed();
        if elapsed >= Self::PERIOD {
            let fps = self.frames as f64 / elapsed.as_secs_f64();
            tracing::info!("{fps:.1} fps (show time {show_time:.1}s)");
            self.frames = 0;
            self.since = Instant::now();
        }
    }
}

impl App {
    pub fn new(show: Show) -> Self {
        // Only wrap the clock when looping is explicitly requested.
        let loop_len = show
            .playback
            .looping
            .then_some(show.playback.duration)
            .flatten();
        let clock = Clock::new(loop_len);
        let autoplay_pending = show.playback.autoplay;
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
            autoplay_pending,
            quit_guard: QuitGuard::default(),
            fatal: None,
            gpu: None,
            contents: Vec::new(),
            windows: Vec::new(),
            by_id: HashMap::new(),
            draw_order,
            stats: FrameStats {
                frames: 0,
                since: Instant::now(),
            },
        }
    }

    /// The error that stopped the engine, if any (see [`App::fatal`]).
    pub fn take_fatal(&mut self) -> Option<anyhow::Error> {
        self.fatal.take()
    }

    /// Stop the engine because of `err`: log it, remember it for the exit
    /// code, and end the event loop.
    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        tracing::error!("{err:#}");
        self.fatal.get_or_insert(err);
        event_loop.exit();
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<()> {
        let layer_count = self.show.layers.len();

        // List the monitors so a show's `"monitor": N` can be set correctly.
        for (i, m) in event_loop.available_monitors().enumerate() {
            let (pos, size) = (m.position(), m.size());
            tracing::info!(
                "monitor {i}: {} — {}x{} at ({}, {}), scale {}",
                m.name().unwrap_or_else(|| "unnamed".into()),
                size.width,
                size.height,
                pos.x,
                pos.y,
                m.scale_factor()
            );
        }

        // 1. Create one window per output.
        let mut created: Vec<Arc<Window>> = Vec::with_capacity(self.show.outputs.len());
        for output in &self.show.outputs {
            let attrs = window_attributes(output, event_loop);
            let window = event_loop.create_window(attrs)?;
            if output.window.fullscreen {
                // Never show a mouse pointer on an audience-facing output.
                // Windowed outputs (previews, setup) keep it for the operator.
                window.set_cursor_visible(false);
            }
            created.push(Arc::new(window));
        }

        // 2. Bring up the GPU on the first window, then load content once.
        let (gpu, first_surface) = Gpu::new(created[0].clone())?;
        tracing::info!("GPU: {}", gpu.adapter_info());

        // A surface or slice larger than the GPU's texture limit can't be
        // configured. Refuse to start with a clear message instead of
        // failing validation inside the frame loop.
        let max = gpu.max_texture_dimension();
        for (window, output) in created.iter().zip(&self.show.outputs) {
            let size = window.inner_size();
            if size.width > max || size.height > max {
                anyhow::bail!(
                    "output '{}': window is {}x{}, larger than this GPU's limit of {max}px per \
                     side; set a smaller window.size (or use fullscreen on a real display)",
                    output.id,
                    size.width,
                    size.height
                );
            }
        }
        self.gpu = Some(gpu);
        self.build_contents();

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

    /// Load every layer's content. A layer whose source fails (missing file,
    /// oversized image, ...) gets a visible placeholder and an error log rather
    /// than taking the whole show down.
    fn build_contents(&mut self) {
        let gpu = self.gpu.as_ref().unwrap();
        let factory = TextureFactory {
            device: &gpu.device,
            queue: &gpu.queue,
            layout: &gpu.texture_bgl,
            sampler: &gpu.sampler,
            max_dimension: gpu.max_texture_dimension(),
        };
        self.contents = self
            .show
            .layers
            .iter()
            .map(|layer| {
                tracing::debug!(layer = %layer.id, "loading content");
                content::build(&layer.source, &factory).unwrap_or_else(|e| {
                    tracing::error!(layer = %layer.id, "{e:#}; showing placeholder");
                    content::placeholder(&factory)
                })
            })
            .collect();
    }

    /// Render all outputs for one show-clock sample, then present them together.
    /// Returns how many outputs were actually presented.
    fn render_frame(&mut self, event_loop: &ActiveEventLoop) -> usize {
        let Some(gpu) = self.gpu.as_ref() else {
            return 0;
        };
        let now = self.clock.now();
        for c in &mut self.contents {
            c.update(now, &gpu.queue);
        }

        let targets: Vec<_> = self
            .windows
            .iter()
            .map(|rw| (&rw.gpu, &self.show.outputs[rw.output_idx]))
            .collect();
        let frame = gpu.draw(&targets, &self.show, now, &self.contents, &self.draw_order);
        for rw in &self.windows {
            rw.window.pre_present_notify();
        }
        let presented = frame.presented();
        let errors = frame.present();
        if presented > 0 {
            if self.autoplay_pending {
                // The first frame (show time 0) is on screen: start the show
                // now, and measure fps from here rather than from load.
                self.autoplay_pending = false;
                self.clock.play();
                self.stats = FrameStats {
                    frames: 0,
                    since: Instant::now(),
                };
                tracing::info!("first frame presented; playback started");
            } else {
                self.stats.tick(now);
            }
        }

        for (idx, err) in errors {
            let rw = &mut self.windows[idx];
            match err {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let size = rw.window.inner_size();
                    gpu.resize_output(&mut rw.gpu, size.width, size.height);
                }
                wgpu::SurfaceError::OutOfMemory => {
                    self.fatal
                        .get_or_insert_with(|| anyhow::anyhow!("GPU out of memory"));
                    tracing::error!("GPU out of memory; exiting");
                    event_loop.exit();
                }
                // Typical for a minimized/occluded window; it would otherwise
                // log at the frame rate.
                wgpu::SurfaceError::Timeout => tracing::debug!(output = idx, "surface timeout"),
                e => tracing::warn!("surface error: {e:?}"),
            }
        }

        if let Some(reason) = self.gpu.as_ref().and_then(Gpu::device_lost) {
            // A lost device (driver reset, GPU removed) can't render again.
            // Exit non-zero so a supervisor restarts the engine. (Rebuilding
            // the GPU state in-process is future work.)
            self.fail(event_loop, anyhow::anyhow!("GPU device lost: {reason}"));
        }
        presented
    }
}

/// Window size used when `window.size` is omitted: the `canvas_rect` size,
/// with width and height swapped for a 90/270° output.
fn default_window_size(output: &crate::show::Output) -> [u32; 2] {
    let w = output.canvas_rect.w.max(1.0) as u32;
    let h = output.canvas_rect.h.max(1.0) as u32;
    if matches!(output.rotation_degrees, 90 | 270) {
        [h, w]
    } else {
        [w, h]
    }
}

/// Build the winit window attributes for one output.
fn window_attributes(
    output: &crate::show::Output,
    event_loop: &ActiveEventLoop,
) -> winit::window::WindowAttributes {
    // Window geometry: explicit window size, else the canvas-rect size —
    // turned by `rotation_degrees`, since the window is the *display* and a
    // 90/270 rotation lays a portrait slice across a landscape display.
    // Without the swap the slice would be squeezed into a portrait window.
    let size = output
        .window
        .size
        .unwrap_or_else(|| default_window_size(output));
    let mut attrs = Window::default_attributes()
        .with_title(format!("lcme · {}", output.id))
        .with_inner_size(PhysicalSize::new(size[0], size[1]));

    if let Some([x, y]) = output.window.position {
        attrs = attrs.with_position(PhysicalPosition::new(x, y));
    }
    if output.window.fullscreen {
        let monitor = match output.window.monitor {
            Some(i) => event_loop.available_monitors().nth(i).or_else(|| {
                tracing::warn!(
                    "output '{}': monitor {i} not found; using the primary monitor",
                    output.id
                );
                event_loop.primary_monitor()
            }),
            None => event_loop.primary_monitor(),
        };
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
            self.fail(event_loop, e.context("initialization failed"));
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

        let locked = self.show.playback.locked;
        match event {
            WindowEvent::CloseRequested if locked => {
                tracing::warn!("window close ignored: the show is locked (stop it with Ctrl+C)");
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_ref() {
                    gpu.resize_output(&mut self.windows[idx].gpu, size.width, size.height);
                }
            }
            // Locked show: the keyboard does nothing. Auto-repeat is ignored
            // for every key, so a held or stuck key acts once, not ~30x/s
            // (holding → used to jump a live show ~150 s per second).
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat && !locked =>
            {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        if self.quit_guard.press(Instant::now()) {
                            event_loop.exit();
                        } else {
                            tracing::warn!(
                                "press Esc again within {:.1} s to quit",
                                QUIT_CONFIRM_WINDOW.as_secs_f64()
                            );
                        }
                    }
                    Key::Named(NamedKey::Space) => self.clock.toggle(),
                    Key::Named(NamedKey::ArrowRight) => self.clock.seek_by(5.0),
                    Key::Named(NamedKey::ArrowLeft) => self.clock.seek_by(-5.0),
                    Key::Named(NamedKey::ArrowUp) => self.clock.seek_by(30.0),
                    Key::Named(NamedKey::ArrowDown) => self.clock.seek_by(-30.0),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Frames are driven from here rather than per-window RedrawRequested so
        // all outputs render together. FIFO present blocks on vsync, which
        // paces the loop, so Poll does not spin the CPU — but only while at
        // least one output presents. With every window minimized (size 0),
        // acquiring returns at once with an error, so back off instead of
        // spinning a core; restore/resize events wake the loop earlier.
        use winit::event_loop::ControlFlow;
        if self.render_frame(event_loop) > 0 {
            event_loop.set_control_flow(ControlFlow::Poll);
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(50),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(extra: &str) -> crate::show::Output {
        let json = format!(
            r#"{{ "canvas": {{ "width": 1080, "height": 1920 }},
                 "outputs": [ {{ "id": "o", "canvas_rect": {{ "x": 0, "y": 0, "w": 1080, "h": 1920 }} {extra} }} ] }}"#
        );
        Show::from_json(&json).unwrap().outputs.remove(0)
    }

    #[test]
    fn quit_needs_two_presses_close_together() {
        let t0 = Instant::now();
        let mut g = QuitGuard::default();
        assert!(!g.press(t0), "a single press must not quit");
        assert!(g.press(t0 + Duration::from_millis(500)));

        let mut g = QuitGuard::default();
        assert!(!g.press(t0));
        // Too late: this press re-arms instead of quitting...
        assert!(!g.press(t0 + Duration::from_secs(3)));
        // ...and a quick follow-up then confirms.
        assert!(g.press(t0 + Duration::from_millis(3500)));
    }

    #[test]
    fn default_window_follows_the_display_orientation() {
        // Portrait slice on an unrotated output: portrait window.
        assert_eq!(default_window_size(&output("")), [1080, 1920]);
        // Same slice on a physically rotated landscape panel: landscape window.
        assert_eq!(
            default_window_size(&output(r#", "rotation_degrees": 90"#)),
            [1920, 1080]
        );
        assert_eq!(
            default_window_size(&output(r#", "rotation_degrees": 270"#)),
            [1920, 1080]
        );
        assert_eq!(
            default_window_size(&output(r#", "rotation_degrees": 180"#)),
            [1080, 1920]
        );
    }
}
