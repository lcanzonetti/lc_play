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
use crate::playhead::Position;
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
    // The state comes off the wire: everything is worked out in i128 so no
    // value can overflow (as in `TransportState::show_time_at`), and the
    // result is clamped to `now` at the end.
    let start = (start_s * NANOS_PER_SEC as f64).round() as i64 as i128; // `as` saturates (NaN → 0)
    let now128 = now as i128;
    if state.playing && state.rate_permille > 0 {
        let rate = state.rate_permille as i128;
        let to_cluster = |show: i128| show * 1000 / rate;
        let anchor_show = state.anchor_show_ns as i128;
        let len = state.loop_ns.filter(|&len| len > 0).map(|len| len as i128);
        let ahead = match len {
            Some(len) => (start - anchor_show.rem_euclid(len)).rem_euclid(len),
            None => start - anchor_show,
        };
        if ahead >= 0 {
            let mut crossing = state.anchor_cluster_ns as i128 + to_cluster(ahead);
            if let Some(len) = len {
                // The most recent pass over the cue's start.
                let lap = to_cluster(len).max(1);
                if crossing <= now128 {
                    crossing += (now128 - crossing) / lap * lap;
                }
            }
            if crossing <= now128 {
                return crossing as i64; // ≤ now, ≥ an i64 anchor: fits
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
    /// Before `window`: fields drop in order, and the swap chain must be
    /// released before its window is destroyed.
    gpu: OutputGpu,
    window: Arc<Window>,
    /// Index into `App::gpus`: the card this output is rendered by.
    gpu_idx: usize,
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
    /// One device per graphics card that drives an output.
    gpus: Vec<Gpu>,
    /// Per card, one entry per layer, parallel to `show.layers`…
    contents: Vec<Vec<Box<dyn Content>>>,
    /// …and the identity of each, for reuse when the show is edited.
    content_keys: Vec<Vec<String>>,
    /// When output health was last handed to the agent.
    health_at: Instant,
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
            gpus: Vec::new(),
            contents: Vec::new(),
            content_keys: Vec::new(),
            health_at: Instant::now(),
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
                    self.captures = self
                        .windows
                        .iter()
                        .map(|rw| {
                            let name = if rw.output.name.is_empty() {
                                &rw.output.id
                            } else {
                                &rw.output.name
                            };
                            self.gpus[rw.gpu_idx].capture_output(&rw.gpu, name, PREVIEW_WIDTH)
                        })
                        .collect();
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
        for gpu in &self.gpus {
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
        // Forget the on-air instants of layers that are gone.
        self.onsets
            .retain(|id, _| show.layers.iter().any(|l| &l.id == id && l.timing.free_run));
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
        for rw in &mut self.windows {
            self.gpus[rw.gpu_idx].set_layer_count(&mut rw.gpu, layers.len());
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
                    let moved = rw.output.window != output.window;
                    if moved || size_follows_area {
                        apply_placement(&rw.window, output, event_loop);
                    }
                    // Moved to a monitor on another card: render it there.
                    if moved && self.gpus.len() > 1 {
                        let wanted = monitor_adapter(&rw.window)
                            .and_then(|l| self.gpus.iter().position(|g| g.luid() == Some(l)));
                        if let Some(g) = wanted.filter(|&g| g != rw.gpu_idx) {
                            let RenderWindow { gpu, window, .. } = rw;
                            drop(gpu); // one swap chain per window: old one first
                            let out_gpu =
                                self.gpus[g].build_output(window.clone(), output, layer_count)?;
                            tracing::info!(display = %output.id, gpu = %self.gpus[g].adapter_info(), "output moved to another card");
                            rw = RenderWindow {
                                gpu: out_gpu,
                                window,
                                gpu_idx: g,
                                output_idx: idx,
                                output: output.clone(),
                            };
                        }
                    }
                    if rw.output.name != output.name {
                        rw.window.set_title(&window_title(output));
                    }
                    let geometry_changed = rw.output.warp != output.warp
                        || rw.output.blend != output.blend
                        || rw.output.rotation_degrees != output.rotation_degrees;
                    if geometry_changed {
                        self.gpus[rw.gpu_idx].update_output(&mut rw.gpu, output);
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
        if !self.gpus.is_empty() {
            let names: Vec<String> = self.gpus.iter().map(|g| g.adapter_info()).collect();
            let outputs = self.windows.len() as u32;
            self.playhead.update_hardware(|h| {
                h.gpu = Some(names.join(" + "));
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
        let gpu_idx = self.gpu_for(&window)?;
        let gpu = &self.gpus[gpu_idx];
        let out_gpu = gpu.build_output(window.clone(), output, layer_count)?;
        tracing::info!(
            display = %output.id,
            gpu = %gpu.adapter_info(),
            presenter = out_gpu.presenter_name(),
            "output opened"
        );
        Ok(RenderWindow {
            window,
            gpu: out_gpu,
            gpu_idx,
            output_idx: idx,
            output: output.clone(),
        })
    }

    /// The card that renders `window`: the one its monitor is connected to
    /// (Windows/D3D12), so no other card has to copy the picture across —
    /// which costs a frame of latency on that output alone and keeps it out
    /// of the present barrier. Elsewhere, and on laptops whose screens hang
    /// off the integrated GPU, the one high-performance card.
    fn gpu_for(&mut self, window: &Arc<Window>) -> anyhow::Result<usize> {
        let luid = monitor_adapter(window);
        if let Some(i) = luid.and_then(|l| self.gpus.iter().position(|g| g.luid() == Some(l))) {
            return Ok(i);
        }
        if !self.gpus.is_empty() {
            // A card of its own only if it is a dedicated D3D12 card we are
            // allowed to use; otherwise the card already rendering. (Checked
            // before building a device: that is expensive.)
            #[cfg(windows)]
            let own_card = luid.and_then(crate::render::dedicated_adapter).is_some();
            #[cfg(not(windows))]
            let own_card = false;
            if !own_card {
                return Ok(0);
            }
        }
        let gpu = Gpu::new(window.clone(), luid)?;
        // The device made may be one we already have (the card asked for was
        // refused), or not identifiable: then use the existing one.
        if !self.gpus.is_empty() {
            match gpu
                .luid()
                .and_then(|l| self.gpus.iter().position(|g| g.luid() == Some(l)))
            {
                Some(i) => return Ok(i),
                None if gpu.luid().is_none() => return Ok(0),
                None => {}
            }
        }
        tracing::info!(
            "GPU: {}{}",
            gpu.adapter_info(),
            if gpu.supports_bc() {
                ""
            } else {
                " — no BC texture support: HAP will be decoded in software"
            }
        );
        self.gpus.push(gpu);
        self.contents.push(Vec::new());
        self.content_keys.push(Vec::new());
        // The new card needs the show's contents too.
        self.rebuild_contents_on(self.gpus.len() - 1);
        Ok(self.gpus.len() - 1)
    }

    /// Load the content of every layer of `self.show`, keeping what was
    /// already loaded for layers whose source did not change. A layer whose
    /// source fails (missing file, oversized image, ...) gets a visible
    /// placeholder and an error log rather than taking the whole show down.
    fn rebuild_contents(&mut self) {
        for g in 0..self.gpus.len() {
            self.rebuild_contents_on(g);
        }
    }

    /// [`rebuild_contents`](Self::rebuild_contents) for one card.
    fn rebuild_contents_on(&mut self, g: usize) {
        let factory = self.gpus[g].factory();

        // What is loaded now, by identity.
        let mut loaded: HashMap<String, Vec<Box<dyn Content>>> = HashMap::new();
        let (keys, contents) = (&mut self.content_keys[g], &mut self.contents[g]);
        for (key, content) in keys.drain(..).zip(contents.drain(..)) {
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
            contents.push(content);
            keys.push(key);
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

    /// Render one frame on every output, card by card.
    fn render_frame(&mut self, event_loop: &ActiveEventLoop) {
        if self.windows.is_empty() {
            // No outputs (the show lost its displays): nothing may keep
            // decoding in the background.
            self.contents.iter_mut().flatten().for_each(|c| c.idle());
            return;
        }
        let mut sampled = None;
        for g in 0..self.gpus.len() {
            if let Some(position) = self.render_on(g, event_loop) {
                sampled.get_or_insert(position);
            }
        }
        if let Some(position) = sampled {
            if let Some(fps) = self.stats.tick() {
                // Reported to the Director with the next health message.
                self.playhead.report_fps(fps as f32);
                tracing::info!(
                    "{fps:.1} fps (show time {:.1}s, frame {})",
                    position.show_seconds(),
                    position.frame
                );
            }
        }
        if self.health_at.elapsed() >= Duration::from_millis(500) {
            self.health_at = Instant::now();
            self.publish_output_health();
        }
    }

    /// Render the outputs of card `g`:
    ///
    /// 1. wait for each output to take a frame (this is where the loop is
    ///    paced by the displays);
    /// 2. work out the refresh the frame will appear on — the next one plus
    ///    the frames queued ahead of it, corrected by what the driver reports
    ///    — and sample the show clock *for that instant*, snapped to the
    ///    Director's refresh grid when frame-locked;
    /// 3. update the contents of the layers this card shows, render, present.
    fn render_on(&mut self, g: usize, event_loop: &ActiveEventLoop) -> Option<Position> {
        let outs: Vec<usize> = (0..self.windows.len())
            .filter(|&w| self.windows[w].gpu_idx == g)
            .collect();
        if outs.is_empty() {
            self.contents[g].iter_mut().for_each(|c| c.idle());
            return None;
        }

        let playhead = self.playhead.clone();
        let local_now = || playhead.clock.local_now_ns();
        let mut frames = Vec::with_capacity(outs.len());
        for &w in &outs {
            match self.gpus[g].acquire(&mut self.windows[w].gpu, local_now) {
                Ok(frame) => frames.push((w, frame)),
                Err(e) => self.surface_error(g, w, e, event_loop),
            }
        }
        if frames.is_empty() {
            return None;
        }

        // The refresh each output's frame will reach, and the instant the
        // card's picture is for (that of its first output: outputs of one
        // card share one sample, which keeps them in lockstep).
        let now = local_now();
        let latency = crate::render::frame_latency() as i64;
        // (prediction without correction, correction, time of the refresh)
        let targets: Vec<Option<(i64, i64, i64)>> = frames
            .iter()
            .map(|(w, _)| {
                let t = &self.windows[*w].gpu.timing;
                let (next, _) = t.clock.next_after(now)?;
                let base = next + latency - 1;
                let correction = t.tracker.correction();
                Some((base, correction, t.clock.time_of(base + correction)?))
            })
            .collect();
        let display_local = targets[0].map_or(now, |(_, _, at)| at);
        let was_on_grid = self.windows[frames[0].0].gpu.timing.grid_offset.is_some();
        let (position, grid_offset) = playhead.position_for_display(display_local, was_on_grid);

        let times = self.layer_times(&position);
        let late = self.update_contents(g, &outs, &position, &times);

        let views: Vec<_> = frames
            .iter()
            .map(|(w, f)| {
                let rw = &self.windows[*w];
                (&rw.gpu, &self.show.outputs[rw.output_idx], &f.view)
            })
            .collect();
        self.gpus[g].render(
            &views,
            &self.show,
            &times,
            &self.contents[g],
            &self.draw_order,
        );
        for (w, _) in &frames {
            self.windows[*w].window.pre_present_notify();
        }
        for ((w, frame), target) in frames.into_iter().zip(targets) {
            let rw = &mut self.windows[w];
            self.gpus[g].present(&mut rw.gpu, frame, target.map(|(b, c, _)| (b, c)));
            rw.gpu.timing.grid_offset = grid_offset;
            if late.contains(&w) {
                rw.gpu.timing.late_decodes += 1;
            }
        }
        Some(position)
    }

    /// Each layer's time: that of the timeline it belongs to — the main
    /// timeline's, or its task's (`None` while that task is off stage).
    fn layer_times(&self, position: &Position) -> Vec<Option<f64>> {
        let now = position.show_seconds();
        let mut task_times: HashMap<&str, Option<f64>> = HashMap::new();
        self.show
            .layers
            .iter()
            .map(|layer| match layer.timeline.as_deref() {
                // Test patterns run on the cluster clock, whatever the
                // transport does.
                _ if layer.cluster_clock => {
                    Some(position.cluster_ns.max(0) as f64 / NANOS_PER_SEC as f64)
                }
                None => Some(now),
                Some(id) => *task_times
                    .entry(id)
                    .or_insert_with(|| self.playhead.task_seconds(id, position.cluster_ns)),
            })
            .collect()
    }

    /// Bring card `g`'s contents to `position`. Only layers that fall on one
    /// of the card's outputs (`outs`) are decoded; the others are idle on
    /// this card. Returns the outputs on which a video layer was late.
    fn update_contents(
        &mut self,
        g: usize,
        outs: &[usize],
        position: &Position,
        times: &[Option<f64>],
    ) -> Vec<usize> {
        let loop_s = position
            .transport
            .loop_ns
            .map(|ns| ns as f64 / NANOS_PER_SEC as f64);
        let areas: Vec<(usize, crate::show::Rect)> = outs
            .iter()
            .map(|&w| (w, self.show.outputs[self.windows[w].output_idx].canvas_rect))
            .collect();
        let factory = self.gpus[g].factory();
        let mut late = Vec::new();
        let layers = self
            .show
            .layers
            .iter()
            .zip(&mut self.contents[g])
            .zip(times);
        for ((layer, c), time) in layers {
            let Some(t) = *time else {
                c.idle(); // its timeline is off stage
                          // Played again later, it goes on air afresh.
                self.onsets.remove(&layer.id);
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
            } else {
                // Free run switched off: switched back on, it starts afresh.
                self.onsets.remove(&layer.id);
            }
            let onset = self.onsets.get(&layer.id).copied();
            let media = |timeline_t: f64| match (layer.timing.free_run, onset) {
                (true, Some(onset)) => (position.cluster_ns - onset) as f64 / NANOS_PER_SEC as f64,
                // About to go on air: its first frame.
                (true, None) => 0.0,
                (false, _) => timeline_t,
            };
            // Not on any of this card's outputs: another card (or Runner)
            // shows it. Decoding it here would only cost time.
            let here: Vec<usize> = areas
                .iter()
                .filter(|(_, r)| overlaps(&layer.transform, r))
                .map(|&(w, _)| w)
                .collect();
            if here.is_empty() {
                c.idle();
                continue;
            }
            // Time until the layer appears, allowing for the main timeline
            // looping back to a cue at its beginning.
            let mut until_start = layer.timing.start - t;
            if let (true, Some(len), None) = (until_start < 0.0, loop_s, &layer.timeline) {
                until_start += len;
            }
            if on_air {
                c.update(media(t), &factory);
                if c.late() {
                    late.extend(here);
                }
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
        late.sort_unstable();
        late.dedup();
        late
    }

    /// React to an output whose frame could not be acquired.
    fn surface_error(
        &mut self,
        g: usize,
        w: usize,
        err: wgpu::SurfaceError,
        event_loop: &ActiveEventLoop,
    ) {
        let rw = &mut self.windows[w];
        match err {
            wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                let size = rw.window.inner_size();
                self.gpus[g].resize_output(&mut rw.gpu, size.width, size.height);
            }
            wgpu::SurfaceError::OutOfMemory => {
                tracing::error!("GPU out of memory; exiting");
                event_loop.exit();
            }
            // Typical for a minimized/occluded window; it would otherwise
            // log at the frame rate.
            wgpu::SurfaceError::Timeout => tracing::debug!(output = w, "surface timeout"),
            e => tracing::warn!("surface error: {e:?}"),
        }
    }

    /// Hand each output's presentation health, and the first output's
    /// refresh (for the Director's grid), to the agent.
    fn publish_output_health(&self) {
        let now = self.playhead.clock.local_now_ns();
        let health: Vec<lc_proto::OutputHealth> = self
            .windows
            .iter()
            .map(|rw| {
                let t = &rw.gpu.timing;
                let exact = rw.gpu.exact_timing();
                lc_proto::OutputHealth {
                    id: rw.output.id.clone(),
                    gpu: self.gpus[rw.gpu_idx].adapter_info(),
                    presenter: rw.gpu.presenter_name().into(),
                    refresh_hz: t.clock.period().map(|p| 1e9 / p),
                    presents: t.tracker.presents,
                    missed: t.tracker.missed,
                    late_decodes: t.late_decodes,
                    latency_frames: exact
                        .then(|| crate::render::frame_latency() as i64 + t.tracker.correction()),
                    barrier: if t.barrier.is_empty() {
                        "off"
                    } else {
                        t.barrier
                    }
                    .into(),
                    grid_offset_ns: t.grid_offset,
                    frame_counter: t.frame_counter,
                }
            })
            .collect();
        let vblank = self.windows.first().and_then(|rw| {
            let clock = &rw.gpu.timing.clock;
            let (_, at) = clock.last_refresh(now)?;
            let period = clock.period()?;
            let reading = self.playhead.clock.read(at);
            reading.synced.then(|| lc_proto::VblankReport {
                vblank_ns: reading.cluster_ns,
                period_ps: (period * 1000.0).round() as i64,
                exact: clock.is_exact(),
            })
        });
        let downloads = match &self.source {
            ShowSource::Cluster { assets, .. } => assets.downloads(),
            ShowSource::Local(_) => Vec::new(),
        };
        self.playhead.update_hardware(|h| {
            h.output_health = health;
            h.vblank = vblank;
            h.downloads = downloads;
        });
    }
}

/// The LUID of the card driving the monitor `window` is on (Windows only).
fn monitor_adapter(window: &Window) -> Option<i64> {
    #[cfg(windows)]
    {
        use winit::platform::windows::MonitorHandleExtWindows;
        window
            .current_monitor()
            .and_then(|m| crate::dxgi::adapter_of_monitor(m.hmonitor()))
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

/// Whether a layer can touch the canvas area `r`. Conservative: the layer is
/// taken as a circle around its anchor point as wide as its diagonal, which
/// covers any rotation and anchor.
fn overlaps(t: &crate::show::Transform, r: &crate::show::Rect) -> bool {
    let reach = (t.size[0] * t.scale[0]).hypot(t.size[1] * t.scale[1]);
    let [x, y] = t.position;
    x + reach >= r.x && x - reach <= r.x + r.w && y + reach >= r.y && y - reach <= r.y + r.h
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
                let rw = &mut self.windows[idx];
                self.gpus[rw.gpu_idx].resize_output(&mut rw.gpu, size.width, size.height);
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
            self.contents.iter_mut().flatten().for_each(|c| c.idle());
            // Downloads still progress (and are reported) without outputs.
            if self.health_at.elapsed() >= Duration::from_millis(500) {
                self.health_at = Instant::now();
                self.publish_output_health();
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
    fn on_air_since_survives_hostile_states() {
        // Values off the wire must not overflow (a debug build would panic).
        for (anchor_cluster, anchor_show, rate, looping) in [
            (i64::MIN, i64::MAX, 1, None),
            (i64::MAX, i64::MIN, 1, Some(i64::MAX)),
            (0, i64::MIN, 1_000_000, Some(1)),
            (i64::MAX - 1, 0, 1, None),
        ] {
            let s = TransportState {
                epoch: 1,
                playing: true,
                rate_permille: rate,
                anchor_cluster_ns: anchor_cluster,
                anchor_show_ns: anchor_show,
                loop_ns: looping,
                active: true,
            };
            for start in [0.0, 1e30, -1e30, f64::NAN] {
                assert!(on_air_since(&s, start, 1_000) <= 1_000);
            }
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
