//! The windowing + event-loop layer (winit 0.30 `ApplicationHandler`).
//!
//! It creates one window per output, brings up the GPU, loads each layer's
//! content, then drives a continuous frame loop. Each
//! frame samples the [`Playhead`] **once** and renders every output from that
//! single timestamp in one GPU submission, so outputs never disagree about
//! show time. Pacing comes from vsync (FIFO present).
//!
//! The playhead is the only link to the rest of the cluster: stand-alone it
//! runs on the local clock, under a Director it follows cluster time — the
//! renderer cannot tell the difference and never waits for the network.
//!
//! The show itself comes from one of two places ([`ShowSource`]): a local
//! show file, fixed for the run; or the cluster, where the Director's show
//! document is re-read whenever it changes, so edits made in the Producer
//! appear on the outputs while the show is running. Windows are only rebuilt
//! when the displays themselves change; loaded media is kept across edits.
//!
//! Keyboard transport (stand-alone only): Space = play/pause, ←/→ = seek ±5s,
//! ↓/↑ = seek ±30s. Esc = quit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use lc_proto::NANOS_PER_SEC;
use lc_time::TransportChange;

use crate::assets::AssetCache;
use crate::content::{self, Content, TextureFactory};
use crate::playhead::Playhead;
use crate::render::{Gpu, OutputGpu};
use crate::show::{Canvas, Output, Playback, Show};
use crate::stage;

/// How long before a layer appears its content starts preparing (seconds).
const PREROLL_S: f64 = 1.0;

/// Where the show being rendered comes from.
pub enum ShowSource {
    /// A show file given on the command line. Never changes.
    Local(Show),
    /// The show document distributed by the Director, restricted to the
    /// displays assigned to `runner_name`.
    Cluster {
        runner_name: String,
        assets: Arc<AssetCache>,
    },
}

struct RenderWindow {
    window: Arc<Window>,
    gpu: OutputGpu,
    /// Index into `show.outputs`.
    output_idx: usize,
}

pub struct App {
    source: ShowSource,
    /// The show as the renderer sees it (empty until a cluster show arrives).
    show: Show,
    playhead: Arc<Playhead>,
    gpu: Option<Gpu>,
    /// One entry per layer, parallel to `show.layers`…
    contents: Vec<Box<dyn Content>>,
    /// …and the identity of each, for reuse when the show is edited.
    content_keys: Vec<String>,
    windows: Vec<RenderWindow>,
    /// The outputs the current windows were built for.
    window_outputs: Vec<Output>,
    by_id: HashMap<WindowId, usize>,
    /// Layer indices sorted back-to-front by z.
    draw_order: Vec<usize>,
    stats: FrameStats,
    /// Cluster only: (show id, asset-cache generation) of what is applied.
    applied: Option<(String, u64)>,
    started: bool,
}

/// Logs the frame rate every few seconds so a show's performance is visible.
struct FrameStats {
    frames: u32,
    since: Instant,
}

impl FrameStats {
    const PERIOD: Duration = Duration::from_secs(5);

    /// Count a frame; every `PERIOD` return the measured frame rate.
    fn tick(&mut self) -> Option<f64> {
        self.frames += 1;
        let elapsed = self.since.elapsed();
        if elapsed < Self::PERIOD {
            return None;
        }
        let fps = self.frames as f64 / elapsed.as_secs_f64();
        self.frames = 0;
        self.since = Instant::now();
        Some(fps)
    }
}

/// A show with nothing in it: what a clustered Runner renders (i.e. no
/// windows at all) until the Director has sent one.
fn empty_show() -> Show {
    Show {
        canvas: Canvas {
            width: 1.0,
            height: 1.0,
            background: [0.0, 0.0, 0.0, 1.0],
        },
        outputs: Vec::new(),
        layers: Vec::new(),
        playback: Playback::default(),
    }
}

impl App {
    pub fn new(source: ShowSource, playhead: Arc<Playhead>) -> Self {
        App {
            source,
            show: empty_show(),
            playhead,
            gpu: None,
            contents: Vec::new(),
            content_keys: Vec::new(),
            windows: Vec::new(),
            window_outputs: Vec::new(),
            by_id: HashMap::new(),
            draw_order: Vec::new(),
            stats: FrameStats {
                frames: 0,
                since: Instant::now(),
            },
            applied: None,
            started: false,
        }
    }

    /// Cluster only: if the Director's show (or the set of downloaded
    /// assets) changed since the last look, rebuild from it.
    fn follow_cluster_show(&mut self, event_loop: &ActiveEventLoop) {
        let ShowSource::Cluster {
            runner_name,
            assets,
        } = &self.source
        else {
            return;
        };
        let Some(doc) = self.playhead.show() else {
            return;
        };
        let version = (doc.id.clone(), assets.generation());
        if self.applied.as_ref() == Some(&version) {
            return;
        }
        // Read the generation *before* building: a download that completes
        // while we build bumps it again and triggers another pass.
        let show = match serde_json::from_value(doc.document.clone()) {
            Ok(document) => stage::build(&document, runner_name, assets),
            Err(e) => {
                tracing::error!(
                    "show revision {} is not a valid document: {e}",
                    doc.revision
                );
                self.applied = Some(version);
                return;
            }
        };
        if self.applied.as_ref().map(|(id, _)| id) != Some(&doc.id) {
            tracing::info!(
                "show revision {}: {} display(s) for this runner, {} cue(s)",
                doc.revision,
                show.outputs.len(),
                show.layers.len()
            );
            if show.outputs.is_empty() {
                tracing::warn!(
                    "the show has no display assigned to runner '{runner_name}': nothing to render"
                );
            }
        }
        self.applied = Some(version);
        self.apply_show(event_loop, show);
    }

    /// Make `show` the one being rendered.
    fn apply_show(&mut self, event_loop: &ActiveEventLoop, show: Show) {
        // Windows are expensive and visible: touch them only if the displays
        // changed. Moving a cue must not make every output blink.
        if show.outputs != self.window_outputs {
            if let Err(e) = self.rebuild_windows(event_loop, &show) {
                tracing::error!("cannot set up the outputs: {e:#}");
                self.windows.clear();
                self.by_id.clear();
                self.window_outputs.clear();
            }
        }
        self.show = show;
        self.rebuild_contents();

        let layers = &self.show.layers;
        self.draw_order = (0..layers.len()).collect();
        self.draw_order.sort_by(|&a, &b| {
            layers[a]
                .transform
                .z
                .partial_cmp(&layers[b].transform.z)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some(gpu) = &self.gpu {
            for rw in &mut self.windows {
                gpu.set_layer_count(&mut rw.gpu, layers.len());
            }
        }
    }

    /// Create one window (and its GPU state) per output of `show`.
    fn rebuild_windows(&mut self, event_loop: &ActiveEventLoop, show: &Show) -> anyhow::Result<()> {
        // Surfaces first, then their windows.
        self.windows.clear();
        self.by_id.clear();
        self.window_outputs.clear();

        let mut created: Vec<Arc<Window>> = Vec::with_capacity(show.outputs.len());
        for output in &show.outputs {
            let attrs = window_attributes(output, event_loop);
            created.push(Arc::new(event_loop.create_window(attrs)?));
        }
        let Some(first) = created.first() else {
            return Ok(()); // no outputs: nothing to draw on
        };

        // The GPU device is created once, against the first window ever
        // opened, and then kept for the lifetime of the process.
        let mut first_surface = None;
        if self.gpu.is_none() {
            let (gpu, surface) = Gpu::new(first.clone())?;
            tracing::info!("GPU: {}", gpu.adapter_info());
            self.gpu = Some(gpu);
            first_surface = Some(surface);
        }
        let gpu = self.gpu.as_ref().expect("gpu was just ensured");

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
                &show.outputs[idx],
                show.layers.len(),
            );
            self.by_id.insert(window.id(), self.windows.len());
            self.windows.push(RenderWindow {
                window,
                gpu: out_gpu,
                output_idx: idx,
            });
        }
        self.window_outputs = show.outputs.clone();
        tracing::info!(outputs = self.windows.len(), "outputs ready");
        Ok(())
    }

    /// Load the content of every layer of `self.show`, keeping what was
    /// already loaded for layers whose source did not change. A layer whose
    /// source fails (missing file, oversized image, ...) gets a visible
    /// placeholder and an error log rather than taking the whole show down.
    fn rebuild_contents(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else {
            // No GPU yet (no window was ever needed): nothing can be loaded,
            // and nothing will be drawn.
            self.contents.clear();
            self.content_keys.clear();
            return;
        };
        let factory = TextureFactory {
            device: &gpu.device,
            queue: &gpu.queue,
            layout: &gpu.texture_bgl,
            sampler: &gpu.sampler,
            max_dimension: gpu.max_texture_dimension(),
        };

        // What is loaded now, by identity.
        let mut loaded: HashMap<String, Vec<Box<dyn Content>>> = HashMap::new();
        for (key, content) in self.content_keys.drain(..).zip(self.contents.drain(..)) {
            loaded.entry(key).or_default().push(content);
        }

        for layer in &self.show.layers {
            let key = content::cache_key(layer);
            let content = match loaded.get_mut(&key).and_then(Vec::pop) {
                Some(mut kept) => {
                    kept.retime(&layer.timing);
                    kept
                }
                None => {
                    tracing::debug!(layer = %layer.id, "loading content");
                    content::build(layer, &factory).unwrap_or_else(|e| {
                        tracing::error!(layer = %layer.id, "{e:#}; showing placeholder");
                        content::placeholder(&factory)
                    })
                }
            };
            self.contents.push(content);
            self.content_keys.push(key);
        }
        // Whatever is left in `loaded` is no longer used and is dropped here
        // (for video that also stops its decoder).
    }

    /// A transport key was pressed. Honoured only on a stand-alone Runner:
    /// under a Director, one node moving its own playhead would put it out
    /// of sync with every other Runner.
    fn local_transport(&mut self, change: TransportChange) {
        if self.playhead.is_clustered() {
            tracing::warn!("transport keys are disabled: this runner follows a Director");
        } else {
            self.playhead.apply_local(change);
        }
    }

    fn seek_by(&mut self, seconds: i64) {
        self.local_transport(TransportChange::SeekBy(seconds * NANOS_PER_SEC));
    }

    /// Render all outputs for one show-clock sample, then present them together.
    fn render_frame(&mut self, event_loop: &ActiveEventLoop) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        if self.windows.is_empty() {
            // No outputs (the show lost its displays): nothing may keep
            // decoding in the background.
            for c in &mut self.contents {
                c.idle();
            }
            return;
        }
        // One sample per frame, shared by every output.
        let position = self.playhead.position();
        let now = position.show_seconds();
        let loop_s = position
            .transport
            .loop_ns
            .map(|ns| ns as f64 / NANOS_PER_SEC as f64);
        for (layer, c) in self.show.layers.iter().zip(&mut self.contents) {
            // Time until the layer appears, allowing for the show looping
            // back to a cue at its beginning.
            let mut until_start = layer.timing.start - now;
            if let (true, Some(len)) = (until_start < 0.0, loop_s) {
                until_start += len;
            }
            if layer.timing.visible_at(now) {
                c.update(now, &gpu.queue);
            } else if (0.0..=PREROLL_S).contains(&until_start) {
                // About to appear: get its first frame decoded and into the
                // texture now, so the cue starts on its first frame instead
                // of on whatever the decoder manages a moment later.
                c.update(layer.timing.start, &gpu.queue);
            } else {
                // Off screen: an invisible video cue must not keep a decoder
                // (an ffmpeg process and several frames of memory) around.
                c.idle();
            }
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
        let errors = frame.present();
        if let Some(fps) = self.stats.tick() {
            // Reported to the Director with the next health message.
            self.playhead.report_fps(fps as f32);
            tracing::info!(
                "{fps:.1} fps (show time {now:.1}s, frame {})",
                position.frame
            );
        }

        for (idx, err) in errors {
            let rw = &mut self.windows[idx];
            match err {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let size = rw.window.inner_size();
                    gpu.resize_output(&mut rw.gpu, size.width, size.height);
                }
                wgpu::SurfaceError::OutOfMemory => {
                    tracing::error!("GPU out of memory; exiting");
                    event_loop.exit();
                }
                // Typical for a minimized/occluded window; it would otherwise
                // log at the frame rate.
                wgpu::SurfaceError::Timeout => tracing::debug!(output = idx, "surface timeout"),
                e => tracing::warn!("surface error: {e:?}"),
            }
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
        .with_title(format!("lc-runner · {}", output.id))
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
        if self.started {
            return;
        }
        self.started = true;

        // List the monitors so a display's `"monitor": N` can be set correctly.
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

        match &self.source {
            ShowSource::Local(show) => {
                let show = show.clone();
                self.apply_show(event_loop, show);
                if self.windows.is_empty() {
                    event_loop.exit(); // a local show without outputs is an error already logged
                }
            }
            ShowSource::Cluster { runner_name, .. } => {
                tracing::info!("runner '{runner_name}' waiting for a show from the Director");
            }
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
                    Key::Named(NamedKey::Space) => {
                        let playing = self.playhead.position().transport.playing;
                        self.local_transport(if playing {
                            TransportChange::Pause
                        } else {
                            TransportChange::Play
                        });
                    }
                    Key::Named(NamedKey::ArrowRight) => self.seek_by(5),
                    Key::Named(NamedKey::ArrowLeft) => self.seek_by(-5),
                    Key::Named(NamedKey::ArrowUp) => self.seek_by(30),
                    Key::Named(NamedKey::ArrowDown) => self.seek_by(-30),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.follow_cluster_show(event_loop);

        if self.windows.is_empty() {
            // Nothing may keep decoding with no output to show it on.
            for c in &mut self.contents {
                c.idle();
            }
            // Nothing to draw (no show yet, or no display for this Runner):
            // wake up a few times a second to look for one, instead of
            // spinning.
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(100),
            ));
            return;
        }
        // Frames are driven from here rather than per-window RedrawRequested so
        // all outputs render together. FIFO present blocks on vsync, which
        // paces the loop, so Poll does not spin the CPU.
        self.render_frame(event_loop);
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    }
}
