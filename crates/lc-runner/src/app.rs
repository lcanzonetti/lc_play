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

use lc_proto::{RunnerAction, NANOS_PER_SEC};
use lc_time::TransportChange;

use crate::assets::AssetCache;
use crate::content::{self, Content};
use crate::playhead::Playhead;
use crate::render::{Capture, Gpu, OutputGpu};
use crate::show::{Canvas, Output, Playback, Show};
use crate::stage;

/// The cluster time at which a cue starting at `start_s` went on air under
/// transport `state`, as seen at cluster time `now`.
///
/// If the playhead reached the cue by playing through its start, that is the
/// exact instant it crossed it — the same on every Runner, since they all
/// hold the same transport state. Otherwise (a seek or jump into the middle
/// of the cue, a stop-and-play) the cue went on air when that state took
/// effect.
pub fn on_air_since(state: &lc_proto::TransportState, start_s: f64, now: i64) -> i64 {
    let start = (start_s * NANOS_PER_SEC as f64).round() as i64;
    if state.playing && state.rate_permille > 0 {
        let rate = state.rate_permille as i128;
        let to_cluster = |show: i64| (show as i128 * 1000 / rate) as i64;
        let ahead = match state.loop_ns.filter(|&len| len > 0) {
            Some(len) => (start - state.anchor_show_ns.rem_euclid(len)).rem_euclid(len),
            None => start - state.anchor_show_ns,
        };
        if ahead >= 0 {
            let mut crossing = state.anchor_cluster_ns + to_cluster(ahead);
            if let Some(len) = state.loop_ns.filter(|&len| len > 0) {
                // The most recent pass over the cue's start.
                let lap = to_cluster(len).max(1);
                if crossing <= now {
                    crossing += (now - crossing) / lap * lap;
                }
            }
            if crossing <= now {
                return crossing;
            }
        }
    }
    state.anchor_cluster_ns.min(now)
}

/// Width of the output pictures sent to the operator, in pixels.
const PREVIEW_WIDTH: u32 = 480;

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
    /// The output as it was last applied to this window.
    output: Output,
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
    /// Reload requests (see `Playhead::request_reload`) acted on so far.
    reloads_done: u64,
    /// Outputs closed on the operator's request.
    standby: bool,
    /// Free-running layers on air: layer id → the cluster time they went on
    /// air (their media runs from there).
    onsets: HashMap<String, i64>,
    /// Output pictures on their way back from the GPU.
    captures: Vec<Capture>,
    /// How the process should end when the event loop stops: 0 to quit,
    /// `RESTART_EXIT_CODE` to be started again by the supervisor.
    pub exit_code: i32,
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
            reloads_done: 0,
            standby: false,
            onsets: HashMap::new(),
            captures: Vec::new(),
            exit_code: 0,
        }
    }

    /// Carry out the operator's requests queued by the agent.
    fn handle_requests(&mut self, event_loop: &ActiveEventLoop) {
        for action in self.playhead.take_requests() {
            match action {
                RunnerAction::Quit => {
                    tracing::info!("quitting on the operator's request");
                    self.exit_code = 0;
                    event_loop.exit();
                }
                RunnerAction::Restart => {
                    tracing::info!("restarting on the operator's request");
                    self.exit_code = crate::machine::RESTART_EXIT_CODE;
                    event_loop.exit();
                }
                RunnerAction::Standby if !self.standby => {
                    tracing::info!("standby: closing the outputs");
                    self.standby = true;
                    self.playhead.set_standby(true);
                    self.windows.clear();
                    self.by_id.clear();
                    self.window_outputs.clear();
                    self.playhead.update_hardware(|h| h.outputs = 0);
                }
                RunnerAction::Resume if self.standby => {
                    tracing::info!("resuming: opening the outputs");
                    self.standby = false;
                    self.playhead.set_standby(false);
                    let show = std::mem::replace(&mut self.show, empty_show());
                    self.apply_show(event_loop, show);
                }
                RunnerAction::SendPreview => {
                    if let Some(gpu) = &self.gpu {
                        self.captures = self
                            .windows
                            .iter()
                            .map(|rw| {
                                let name = if rw.output.name.is_empty() {
                                    &rw.output.id
                                } else {
                                    &rw.output.name
                                };
                                gpu.capture_output(&rw.gpu, name, PREVIEW_WIDTH)
                            })
                            .collect();
                    }
                    if self.captures.is_empty() {
                        // Nothing to show: answer anyway, so the operator
                        // sees that there are no outputs.
                        self.playhead.put_previews(Vec::new());
                    }
                }
                _ => {}
            }
        }
        self.finish_captures();
    }

    /// Hand finished output pictures to the agent (encoded off this thread).
    fn finish_captures(&mut self) {
        if self.captures.is_empty() {
            return;
        }
        if let Some(gpu) = &self.gpu {
            gpu.poll();
        }
        let all_done = self.captures.iter().all(|c| c.take_ready());
        let too_old = self
            .captures
            .first()
            .is_some_and(|c| c.started.elapsed() > std::time::Duration::from_secs(3));
        if !all_done && !too_old {
            return;
        }
        let pictures: Vec<(String, u32, u32, Vec<u8>)> = self
            .captures
            .drain(..)
            .filter_map(|c| {
                c.take()
                    .map(|rgba| (c.name.clone(), c.width, c.height, rgba))
            })
            .collect();
        let playhead = self.playhead.clone();
        std::thread::spawn(move || {
            let outputs = pictures
                .into_iter()
                .filter_map(|(name, width, height, rgba)| {
                    let mut png = Vec::new();
                    image::ImageEncoder::write_image(
                        image::codecs::png::PngEncoder::new(&mut png),
                        &rgba,
                        width,
                        height,
                        image::ExtendedColorType::Rgba8,
                    )
                    .ok()?;
                    Some(lc_proto::OutputPreview {
                        name,
                        width,
                        height,
                        png_base64: base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            png,
                        ),
                    })
                })
                .collect();
            playhead.put_previews(outputs);
        });
    }

    /// The operator asked for the outputs to be rebuilt: close every window,
    /// drop every loaded texture and decoder, and build it all again from
    /// the current show — what restarting the Runner would do, without
    /// losing the cluster connection or the clock lock.
    fn reload_if_requested(&mut self, event_loop: &ActiveEventLoop) {
        let requested = self.playhead.reload_requests();
        if requested == self.reloads_done {
            return;
        }
        self.reloads_done = requested;
        tracing::info!("reloading outputs");
        self.contents.clear();
        self.content_keys.clear();
        self.windows.clear();
        self.by_id.clear();
        self.window_outputs.clear();
        match &self.source {
            ShowSource::Local(show) => {
                let show = show.clone();
                self.apply_show(event_loop, show);
            }
            // Forget what was applied: the next look at the Director's show
            // rebuilds everything.
            ShowSource::Cluster { .. } => self.applied = None,
        }
        self.report_monitors(event_loop);
    }

    /// Tell the cluster which monitors this machine has (they can change:
    /// displays get plugged in after start-up).
    fn report_monitors(&self, event_loop: &ActiveEventLoop) {
        let primary = event_loop.primary_monitor();
        let monitors = event_loop
            .available_monitors()
            .enumerate()
            .map(|(index, m)| lc_proto::MonitorInfo {
                index,
                primary: primary.as_ref() == Some(&m),
                name: m.name().unwrap_or_else(|| "unnamed".into()),
                width: m.size().width,
                height: m.size().height,
                x: m.position().x,
                y: m.position().y,
                scale: m.scale_factor(),
            })
            .collect();
        self.playhead.update_hardware(|h| h.monitors = monitors);
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
        // In standby the outputs stay closed, whatever the show says.
        let wanted: &[Output] = if self.standby { &[] } else { &show.outputs };
        if wanted != self.window_outputs.as_slice() {
            if let Err(e) = self.sync_windows(event_loop, wanted, show.layers.len()) {
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

    /// Bring the windows in line with the outputs of `show`.
    ///
    /// A display that already has a window keeps it: a changed position,
    /// size, fullscreen state or monitor is applied to the window, and a
    /// changed warp, blend or rotation to its GPU state — the window is
    /// never closed and reopened for an edit. (Closing and reopening every
    /// output each time a display was nudged in the Producer made them
    /// flicker and stall.) Only new displays get new windows, and removed
    /// ones are closed.
    fn sync_windows(
        &mut self,
        event_loop: &ActiveEventLoop,
        outputs: &[Output],
        layer_count: usize,
    ) -> anyhow::Result<()> {
        let mut existing: HashMap<String, RenderWindow> = self
            .windows
            .drain(..)
            .map(|rw| (rw.output.id.clone(), rw))
            .collect();
        self.by_id.clear();
        self.window_outputs.clear();

        for (idx, output) in outputs.iter().enumerate() {
            let rw = match existing.remove(&output.id) {
                Some(mut rw) => {
                    // A windowed display without an explicit size is as big
                    // as the canvas area it shows: resizing that area
                    // resizes the window.
                    let size_follows_area = !output.window.fullscreen
                        && output.window.size.is_none()
                        && (rw.output.canvas_rect.w, rw.output.canvas_rect.h)
                            != (output.canvas_rect.w, output.canvas_rect.h);
                    if rw.output.window != output.window || size_follows_area {
                        apply_placement(&rw.window, output, event_loop);
                    }
                    if rw.output.name != output.name {
                        rw.window.set_title(&window_title(output));
                    }
                    let geometry_changed = rw.output.warp != output.warp
                        || rw.output.blend != output.blend
                        || rw.output.rotation_degrees != output.rotation_degrees;
                    if geometry_changed {
                        if let Some(gpu) = &self.gpu {
                            gpu.update_output(&mut rw.gpu, output);
                        }
                    }
                    rw.output = output.clone();
                    rw.output_idx = idx;
                    rw
                }
                None => self.open_window(event_loop, output, idx, layer_count)?,
            };
            self.by_id.insert(rw.window.id(), self.windows.len());
            self.windows.push(rw);
        }
        // What is left in `existing` belongs to removed displays: dropping it
        // closes those windows.
        drop(existing);

        self.window_outputs = outputs.to_vec();
        if let Some(gpu) = &self.gpu {
            let (gpu_name, outputs) = (gpu.adapter_info(), self.windows.len() as u32);
            self.playhead.update_hardware(|h| {
                h.gpu = Some(gpu_name);
                h.outputs = outputs;
            });
        }
        tracing::info!(outputs = self.windows.len(), "outputs ready");
        Ok(())
    }

    /// Open the window for one output, and its GPU state (bringing up the
    /// GPU itself with the first window ever opened).
    fn open_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        output: &Output,
        idx: usize,
        layer_count: usize,
    ) -> anyhow::Result<RenderWindow> {
        let window = Arc::new(event_loop.create_window(window_attributes(output, event_loop))?);
        let surface = match &self.gpu {
            Some(gpu) => gpu.create_surface(window.clone())?,
            None => {
                let (gpu, surface) = Gpu::new(window.clone())?;
                tracing::info!(
                    "GPU: {}{}",
                    gpu.adapter_info(),
                    if gpu.supports_bc() {
                        ""
                    } else {
                        " — no BC texture support: HAP will be decoded in software"
                    }
                );
                self.gpu = Some(gpu);
                surface
            }
        };
        let gpu = self.gpu.as_ref().expect("gpu was just ensured");
        let phys = window.inner_size();
        let out_gpu = gpu.build_output(surface, phys.width, phys.height, output, layer_count);
        Ok(RenderWindow {
            window,
            gpu: out_gpu,
            output_idx: idx,
            output: output.clone(),
        })
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
        let factory = gpu.factory();

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
        // One sample of the cluster clock per frame, shared by every output
        // and every timeline.
        let position = self.playhead.position();
        let now = position.show_seconds();
        let loop_s = position
            .transport
            .loop_ns
            .map(|ns| ns as f64 / NANOS_PER_SEC as f64);

        // Each layer's time is that of the timeline it belongs to: the main
        // timeline's, or its task's — `None` while that task is off stage.
        let mut task_times: HashMap<&str, Option<f64>> = HashMap::new();
        let times: Vec<Option<f64>> = self
            .show
            .layers
            .iter()
            .map(|layer| match layer.timeline.as_deref() {
                None => Some(now),
                Some(id) => *task_times
                    .entry(id)
                    .or_insert_with(|| self.playhead.task_seconds(id, position.cluster_ns)),
            })
            .collect();

        let factory = gpu.factory();
        for ((layer, c), time) in self.show.layers.iter().zip(&mut self.contents).zip(&times) {
            let Some(t) = *time else {
                c.idle(); // its timeline is off stage
                continue;
            };
            // A free-running layer is asked for media time: seconds since it
            // went on air, at normal speed, whatever its timeline does.
            let on_air = layer.timing.visible_at(t);
            if layer.timing.free_run {
                if on_air {
                    if !self.onsets.contains_key(&layer.id) {
                        let state = match layer.timeline.as_deref() {
                            None => Some(position.transport),
                            Some(id) => self.playhead.task_state(id, position.cluster_ns),
                        };
                        let onset = state.map_or(position.cluster_ns, |s| {
                            on_air_since(&s, layer.timing.start, position.cluster_ns)
                        });
                        self.onsets.insert(layer.id.clone(), onset);
                    }
                } else {
                    self.onsets.remove(&layer.id);
                }
            }
            let onset = self.onsets.get(&layer.id).copied();
            let media = |timeline_t: f64| match (layer.timing.free_run, onset) {
                (true, Some(onset)) => (position.cluster_ns - onset) as f64 / NANOS_PER_SEC as f64,
                // About to go on air: its first frame.
                (true, None) => 0.0,
                (false, _) => timeline_t,
            };
            // Time until the layer appears, allowing for the main timeline
            // looping back to a cue at its beginning.
            let mut until_start = layer.timing.start - t;
            if let (true, Some(len), None) = (until_start < 0.0, loop_s, &layer.timeline) {
                until_start += len;
            }
            if layer.timing.visible_at(t) {
                c.update(media(t), &factory);
            } else if (0.0..=PREROLL_S).contains(&until_start) {
                // About to appear: get its first frame decoded and into the
                // texture now, so the cue starts on its first frame instead
                // of on whatever the decoder manages a moment later.
                c.update(media(layer.timing.start), &factory);
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
        let frame = gpu.draw(
            &targets,
            &self.show,
            &times,
            &self.contents,
            &self.draw_order,
        );
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
    output: &Output,
    event_loop: &ActiveEventLoop,
) -> winit::window::WindowAttributes {
    let [w, h] = window_size(output);
    let mut attrs = Window::default_attributes()
        .with_title(window_title(output))
        .with_inner_size(PhysicalSize::new(w, h));
    if let Some([x, y]) = output.window.position {
        attrs = attrs.with_position(PhysicalPosition::new(x, y));
    }
    attrs.with_fullscreen(fullscreen_target(output, event_loop))
}

fn window_title(output: &Output) -> String {
    let name = if output.name.is_empty() {
        &output.id
    } else {
        &output.name
    };
    format!("LC Play · {name}")
}

/// Window size: the explicit one, else the size of the canvas area shown.
fn window_size(output: &Output) -> [u32; 2] {
    output.window.size.unwrap_or([
        output.canvas_rect.w.max(1.0) as u32,
        output.canvas_rect.h.max(1.0) as u32,
    ])
}

/// Borderless fullscreen on the configured monitor, or `None` (windowed).
fn fullscreen_target(output: &Output, event_loop: &ActiveEventLoop) -> Option<Fullscreen> {
    if !output.window.fullscreen {
        return None;
    }
    let monitor = match output.window.monitor {
        Some(i) => event_loop.available_monitors().nth(i).or_else(|| {
            tracing::warn!(
                "display '{}': monitor {i} not found (this machine has {}); using the primary monitor",
                output.id,
                event_loop.available_monitors().count()
            );
            event_loop.primary_monitor()
        }),
        None => event_loop.primary_monitor(),
    };
    Some(Fullscreen::Borderless(monitor))
}

/// Apply a changed window placement to an open window.
fn apply_placement(window: &Window, output: &Output, event_loop: &ActiveEventLoop) {
    let fullscreen = fullscreen_target(output, event_loop);
    let going_windowed = fullscreen.is_none() && window.fullscreen().is_some();
    window.set_fullscreen(fullscreen);
    if !output.window.fullscreen {
        let [w, h] = window_size(output);
        let _ = window.request_inner_size(PhysicalSize::new(w, h));
        if let Some([x, y]) = output.window.position {
            window.set_outer_position(PhysicalPosition::new(x, y));
        } else if going_windowed {
            window.set_outer_position(PhysicalPosition::new(60, 60));
        }
    }
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

        self.report_monitors(event_loop);
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
        self.handle_requests(event_loop);
        self.reload_if_requested(event_loop);
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

#[cfg(test)]
mod tests {
    use super::*;
    use lc_proto::TransportState;

    const S: i64 = NANOS_PER_SEC;

    fn playing(anchor_cluster_ns: i64, anchor_show_ns: i64) -> TransportState {
        TransportState {
            epoch: 1,
            playing: true,
            anchor_cluster_ns,
            anchor_show_ns,
            ..TransportState::STOPPED
        }
    }

    #[test]
    fn a_free_running_cue_goes_on_air_when_the_playhead_crosses_its_start() {
        // Play from 0 at cluster time 100 s; the cue starts at 3 s of show
        // time, so it went on air at cluster time 103 s.
        assert_eq!(on_air_since(&playing(100 * S, 0), 3.0, 104 * S), 103 * S);
        // Seeked into the middle of the cue: on air since that seek.
        assert_eq!(
            on_air_since(&playing(100 * S, 5 * S), 3.0, 104 * S),
            100 * S
        );
        // Paused (here: the state in which it became visible is a pause).
        let paused = TransportState {
            playing: false,
            ..playing(100 * S, 4 * S)
        };
        assert_eq!(on_air_since(&paused, 3.0, 104 * S), 100 * S);
        // Looping every 10 s: the most recent pass over the start.
        let looping = TransportState {
            loop_ns: Some(10 * S),
            ..playing(100 * S, 0)
        };
        assert_eq!(on_air_since(&looping, 3.0, 125 * S), 123 * S);
    }
}
