//! The windowing + event-loop layer (winit 0.30 `ApplicationHandler`).
//!
//! It creates one window per output and brings up a [`Card`] for each
//! graphics card that drives one — the card the output's monitor is
//! connected to. Each card renders its outputs on its own thread (see
//! [`crate::card`]); every frame of every card is rendered from the
//! machine's frame plan for its refresh ([`crate::plan`]), so outputs never
//! disagree about show time, nor — when a decoder is late — about which
//! frame of a layer to show. Pacing comes from vsync.
//!
//! This thread (the window thread) keeps the windows, the show and the
//! operator's requests; it changes a card only while holding that card's
//! lock, between two of the card's frames.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use lc_proto::{RunnerAction, NANOS_PER_SEC};
use lc_time::TransportChange;

use crate::assets::AssetCache;
use crate::card::{self, Card, CardGuard, CardHandle, Scene, Shared};
use crate::plan::Planner;
use crate::playhead::Playhead;
use crate::render::{Capture, Gpu};
use crate::show::{Output, Show};
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

/// A window, and the card that renders it.
struct RenderWindow {
    window: Arc<Window>,
    /// Index into `App::cards`.
    card: usize,
    /// The output as it was last applied to this window.
    output: Output,
}

pub struct App {
    source: ShowSource,
    /// The show as the renderer sees it (empty until a cluster show arrives).
    show: Arc<Show>,
    /// The playhead, the frame planner, and the flags the cards set.
    shared: Arc<Shared>,
    /// One per graphics card that drives an output (see [`crate::card`]).
    cards: Vec<CardHandle>,
    /// The cards' render threads (none with `LCPLAY_CARD_THREADS=0`).
    threads: Vec<std::thread::JoinHandle<()>>,
    /// Whether cards render on threads of their own, or in turn on this one.
    threaded: bool,
    /// When output health was last handed to the agent.
    health_at: Instant,
    windows: Vec<RenderWindow>,
    /// The outputs the current windows were built for.
    window_outputs: Vec<Output>,
    by_id: HashMap<WindowId, usize>,
    /// Cluster only: (show id, asset-cache generation) of what is applied.
    applied: Option<(String, u64)>,
    started: bool,
    /// Reload requests (see `Playhead::request_reload`) acted on so far.
    reloads_done: u64,
    /// Outputs closed on the operator's request.
    standby: bool,
    /// Output pictures on their way back from the GPU.
    captures: Vec<Capture>,
    /// How the process should end when the event loop stops: 0 to quit,
    /// `RESTART_EXIT_CODE` to be started again by the supervisor.
    pub exit_code: i32,
}

/// One device per output, even for outputs on the same card
/// (`LCPLAY_SPLIT_CARDS=1`): a test aid that runs the code for several
/// cards on a machine with one, e.g. a Linux box with a software
/// rasterizer.
fn split_cards() -> bool {
    std::env::var("LCPLAY_SPLIT_CARDS").is_ok_and(|v| v == "1")
}

impl App {
    pub fn new(source: ShowSource, playhead: Arc<Playhead>) -> Self {
        let threaded = card::threads_enabled();
        if !threaded {
            tracing::info!("cards render in turn on one thread (LCPLAY_CARD_THREADS=0)");
        }
        App {
            source,
            show: Arc::new(Show::empty()),
            shared: Arc::new(Shared {
                playhead,
                planner: Planner::default(),
                stop: AtomicBool::new(false),
                fatal: AtomicBool::new(false),
            }),
            cards: Vec::new(),
            threads: Vec::new(),
            threaded,
            health_at: Instant::now(),
            windows: Vec::new(),
            window_outputs: Vec::new(),
            by_id: HashMap::new(),
            applied: None,
            started: false,
            reloads_done: 0,
            standby: false,
            captures: Vec::new(),
            exit_code: 0,
        }
    }

    fn playhead(&self) -> &Arc<Playhead> {
        &self.shared.playhead
    }

    /// Take every card's lock, in order (waiting at most for each card's
    /// frame in progress).
    fn lock_all(&self) -> Vec<CardGuard> {
        self.cards.iter().map(|c| c.lock_arc()).collect()
    }

    /// Start a render thread for each card that has none yet.
    fn start_threads(&mut self) {
        if !self.threaded {
            return;
        }
        while self.threads.len() < self.cards.len() {
            let card = self.cards[self.threads.len()].clone();
            match card::spawn(card, self.shared.clone()) {
                Ok(thread) => self.threads.push(thread),
                Err(e) => {
                    tracing::error!(
                        "cannot start a render thread ({e}); rendering every card on one thread"
                    );
                    self.stop_threads();
                    self.threaded = false;
                    return;
                }
            }
        }
    }

    /// End the render threads.
    fn stop_threads(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.shared.stop.store(false, Ordering::Relaxed);
    }

    /// Carry out the operator's requests queued by the agent.
    fn handle_requests(&mut self, event_loop: &ActiveEventLoop) {
        for action in self.playhead().take_requests() {
            match action {
                RunnerAction::Quit => {
                    tracing::info!("quitting on the operator's request");
                    self.exit_code = 0;
                    event_loop.exit();
                }
                RunnerAction::Exit => {
                    tracing::info!("exiting for good on the operator's request");
                    self.exit_code = crate::machine::EXIT_FOR_GOOD_CODE;
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
                    self.playhead().set_standby(true);
                    self.close_outputs();
                    self.playhead().update_hardware(|h| h.outputs = 0);
                }
                RunnerAction::Resume if self.standby => {
                    tracing::info!("resuming: opening the outputs");
                    self.standby = false;
                    self.playhead().set_standby(false);
                    let show = self.show.clone();
                    self.apply_show(event_loop, show);
                }
                RunnerAction::SendPreview => {
                    let mut guards = self.lock_all();
                    if let Some(g) = guards.first_mut() {
                        g.timing.note("preview requested");
                    }
                    self.captures = self
                        .windows
                        .iter()
                        .filter_map(|rw| {
                            let name = if rw.output.name.is_empty() {
                                &rw.output.id
                            } else {
                                &rw.output.name
                            };
                            let card = &guards[rw.card];
                            let out = card.output(&rw.output.id)?;
                            Some(card.gpu.capture_output(&out.gpu, name, PREVIEW_WIDTH))
                        })
                        .collect();
                    drop(guards);
                    if self.captures.is_empty() {
                        // Nothing to show: answer anyway, so the operator
                        // sees that there are no outputs.
                        self.playhead().put_previews(Vec::new());
                    }
                }
                _ => {}
            }
        }
        self.finish_captures();
    }

    /// Close every window. Each card lets go of its outputs first, so the
    /// swap chains are released before their windows are destroyed (here,
    /// on the window thread).
    fn close_outputs(&mut self) {
        for mut g in self.lock_all() {
            g.outputs.clear();
        }
        self.windows.clear();
        self.by_id.clear();
        self.window_outputs.clear();
    }

    /// Hand finished output pictures to the agent (encoded off this thread).
    fn finish_captures(&mut self) {
        if self.captures.is_empty() {
            return;
        }
        for g in self.lock_all() {
            g.gpu.poll();
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
        let playhead = self.playhead().clone();
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
        let requested = self.playhead().reload_requests();
        if requested == self.reloads_done {
            return;
        }
        self.reloads_done = requested;
        tracing::info!("reloading outputs");
        // The cards stay; what they hold goes.
        for mut g in self.lock_all() {
            g.timing.note("outputs reloaded");
            g.clear();
        }
        self.close_outputs();
        match &self.source {
            ShowSource::Local(show) => {
                let show = Arc::new(show.clone());
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
        self.playhead().update_hardware(|h| h.monitors = monitors);
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
        let Some(doc) = self.playhead().show() else {
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
        self.apply_show(event_loop, Arc::new(show));
    }

    /// Make `show` the one being rendered.
    ///
    /// Every card's lock is held throughout: no card renders a frame of the
    /// old show with the new plan, or the other way round.
    fn apply_show(&mut self, event_loop: &ActiveEventLoop, show: Arc<Show>) {
        let mut guards = self.lock_all();
        for g in &mut guards {
            g.timing.note("show applied");
        }
        // Windows are expensive and visible: touch them only if the displays
        // changed. Moving a cue must not make every output blink.
        // In standby the outputs stay closed, whatever the show says.
        let wanted: &[Output] = if self.standby { &[] } else { &show.outputs };
        if wanted != self.window_outputs.as_slice() {
            for g in &mut guards {
                g.timing.note("windows rebuilt");
            }
            if let Err(e) = self.sync_windows(event_loop, &mut guards, wanted, show.layers.len()) {
                tracing::error!("cannot set up the outputs: {e:#}");
                for g in &mut guards {
                    g.outputs.clear();
                }
                self.windows.clear();
                self.by_id.clear();
                self.window_outputs.clear();
            }
        }
        let generation = self.shared.planner.set_show(show.clone());
        let scene = Arc::new(Scene::new(show.clone(), generation));
        for g in &mut guards {
            g.set_scene(scene.clone());
        }
        drop(guards);
        self.show = show;
        self.start_threads();
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
        guards: &mut Vec<CardGuard>,
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

        for output in outputs {
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
                    if moved && guards.len() > 1 {
                        let wanted = monitor_adapter(&rw.window)
                            .and_then(|l| guards.iter().position(|g| g.gpu.luid() == Some(l)));
                        if let Some(g) = wanted.filter(|&g| g != rw.card) {
                            // One swap chain per window: the old one first.
                            guards[rw.card].remove_output(&output.id);
                            guards[g].add_output(rw.window.clone(), output, layer_count)?;
                            tracing::info!(display = %output.id, gpu = %guards[g].gpu.adapter_info(), "output moved to another card");
                            rw.card = g;
                        }
                    }
                    if rw.output.name != output.name {
                        rw.window.set_title(&window_title(output));
                    }
                    guards[rw.card].update_output(&output.id, output);
                    rw.output = output.clone();
                    rw
                }
                None => self.open_window(event_loop, guards, output, layer_count)?,
            };
            self.by_id.insert(rw.window.id(), self.windows.len());
            self.windows.push(rw);
        }
        // What is left in `existing` belongs to removed displays: their
        // cards let go of them, then dropping them closes the windows.
        for (id, rw) in existing {
            guards[rw.card].remove_output(&id);
            drop(rw);
        }

        self.window_outputs = outputs.to_vec();
        if !guards.is_empty() {
            let names: Vec<String> = guards.iter().map(|g| g.gpu.adapter_info()).collect();
            let outputs = self.windows.len() as u32;
            self.playhead().update_hardware(|h| {
                h.gpu = Some(names.join(" + "));
                h.outputs = outputs;
            });
        }
        tracing::info!(
            outputs = self.windows.len(),
            cards = guards.len(),
            "outputs ready"
        );
        Ok(())
    }

    /// Open the window for one output, on the card its monitor is connected
    /// to (bringing that card up the first time).
    fn open_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        guards: &mut Vec<CardGuard>,
        output: &Output,
        layer_count: usize,
    ) -> anyhow::Result<RenderWindow> {
        let window = Arc::new(event_loop.create_window(window_attributes(output, event_loop))?);
        let card = self.card_for(guards, &window)?;
        guards[card].add_output(window.clone(), output, layer_count)?;
        Ok(RenderWindow {
            window,
            card,
            output: output.clone(),
        })
    }

    /// The card that renders `window`: the one its monitor is connected to
    /// (Windows/D3D12), so no other card has to copy the picture across —
    /// which costs a frame of latency on that output alone and keeps it out
    /// of the present barrier. Elsewhere, and on laptops whose screens hang
    /// off the integrated GPU, the one high-performance card.
    fn card_for(
        &mut self,
        guards: &mut Vec<CardGuard>,
        window: &Arc<Window>,
    ) -> anyhow::Result<usize> {
        if split_cards() && !guards.is_empty() {
            let gpu = Gpu::new(window.clone(), None)?;
            tracing::info!(
                "GPU: {} (one more device: LCPLAY_SPLIT_CARDS=1)",
                gpu.adapter_info()
            );
            return Ok(self.add_card(guards, gpu));
        }
        let luid = monitor_adapter(window);
        if let Some(i) = luid.and_then(|l| guards.iter().position(|g| g.gpu.luid() == Some(l))) {
            return Ok(i);
        }
        if !guards.is_empty() {
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
        if !guards.is_empty() {
            match gpu
                .luid()
                .and_then(|l| guards.iter().position(|g| g.gpu.luid() == Some(l)))
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
        Ok(self.add_card(guards, gpu))
    }

    /// Bring a new card into use. Its lock is added to `guards`; its render
    /// thread starts once the show has been applied to it.
    fn add_card(&mut self, guards: &mut Vec<CardGuard>, gpu: Gpu) -> usize {
        let index = self.cards.len();
        let handle: CardHandle = Arc::new(parking_lot::Mutex::new(Card::new(index, gpu)));
        guards.push(handle.lock_arc());
        self.cards.push(handle);
        index
    }

    /// A transport key was pressed. Honoured only on a stand-alone Runner:
    /// under a Director, one node moving its own playhead would put it out
    /// of sync with every other Runner.
    fn local_transport(&mut self, change: TransportChange) {
        if self.playhead().is_clustered() {
            tracing::warn!("transport keys are disabled: this runner follows a Director");
        } else {
            self.playhead().apply_local(change);
        }
    }

    fn seek_by(&mut self, seconds: i64) {
        self.local_transport(TransportChange::SeekBy(seconds * NANOS_PER_SEC));
    }

    /// Hand each output's presentation health, and the first output's
    /// refresh (for the Director's grid), to the agent. Also keeps each
    /// output's refresh clock told the display mode's rate: a window can be
    /// moved to another monitor, or the mode changed.
    fn publish_output_health(&self) {
        let now = self.playhead().clock.local_now_ns();
        let mut guards = self.lock_all();
        for rw in &self.windows {
            // The window system can briefly not know the monitor (while a
            // window moves): then the last known mode stands.
            if let Some(nominal) = crate::render::nominal_period(&rw.window) {
                if let Some(o) = guards[rw.card].output_mut(&rw.output.id) {
                    o.gpu.timing.clock.set_nominal(Some(nominal));
                }
            }
        }
        let health: Vec<lc_proto::OutputHealth> = self
            .windows
            .iter()
            .filter_map(|rw| guards[rw.card].health(&rw.output.id))
            .collect();
        let vblank = self.windows.first().and_then(|rw| {
            let clock = &guards[rw.card].output(&rw.output.id)?.gpu.timing.clock;
            // A young or foreign estimate would become every Runner's grid.
            if !clock.is_mature() {
                return None;
            }
            let (_, at) = clock.last_refresh(now)?;
            let period = clock.period()?;
            let reading = self.playhead().clock.read(at);
            reading.synced.then(|| lc_proto::VblankReport {
                vblank_ns: reading.cluster_ns,
                period_ps: (period * 1000.0).round() as i64,
                exact: clock.is_exact(),
            })
        });
        drop(guards);
        let downloads = match &self.source {
            ShowSource::Cluster { assets, .. } => assets.downloads(),
            ShowSource::Local(_) => Vec::new(),
        };
        self.playhead().update_hardware(|h| {
            h.output_health = health;
            h.vblank = vblank;
            h.downloads = downloads;
        });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Render threads first: then every card, output and window is
        // released here, on the window thread.
        self.stop_threads();
        self.close_outputs();
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
                let show = Arc::new(show.clone());
                self.apply_show(event_loop, show);
                if self.windows.is_empty() {
                    // A local show without outputs is an error, already
                    // logged.
                    self.exit_code = 1;
                    event_loop.exit();
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
                let rw = &self.windows[idx];
                self.cards[rw.card]
                    .lock()
                    .resize(&rw.output.id, size.width, size.height);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::Space) => {
                        let playing = self.playhead().position().transport.playing;
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
        if self.shared.fatal.load(Ordering::Relaxed) {
            // Not a close: the supervisor restarts it.
            self.exit_code = 1;
            event_loop.exit();
            return;
        }
        // Downloads still progress (and are reported) without outputs.
        if self.health_at.elapsed() >= Duration::from_millis(500) {
            self.health_at = Instant::now();
            self.publish_output_health();
        }

        if self.windows.is_empty() {
            if !self.threaded {
                // Nothing may keep decoding with no output to show it on.
                // (Render threads see to that themselves.)
                for mut g in self.lock_all() {
                    g.idle();
                }
            }
            // Nothing to draw (no show yet, or no display for this Runner):
            // wake up a few times a second to look for one, instead of
            // spinning.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(100),
            ));
            return;
        }
        if self.threaded {
            // The cards render on their own threads; this one only keeps
            // the windows, the show and the operator's requests going.
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(10),
            ));
            return;
        }
        // One thread for every card, in turn. Presenting blocks on vsync,
        // which paces the loop, so Poll does not spin the CPU.
        let mut rendered = false;
        for card in &self.cards {
            rendered |= card.lock().step(&self.shared);
        }
        event_loop.set_control_flow(if rendered {
            ControlFlow::Poll
        } else {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(10))
        });
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
