//! One graphics card's share of the rendering: its device, the contents of
//! every layer as that card holds them, its virtual displays and the outputs
//! connected to it — and the loop that renders those outputs.
//!
//! ## One thread per card
//!
//! Each card is driven by its own render thread ([`spawn`]): acquire a
//! frame on each of its outputs (which waits for the display), ask the
//! machine's [`Planner`] what every layer shows on that refresh, update the
//! layers that fall on this card's outputs, render, present. A card never
//! waits for another card's refresh, uploads or encoding — which a single
//! loop driving every card in turn had to do.
//!
//! The window thread still owns the windows and the show. It changes a card
//! (a new show, outputs opened, moved or resized, a preview requested,
//! health read) by taking the card's lock between two of its frames. A card
//! holds its lock for a whole frame, including the wait for its displays
//! (that wait cannot be split from acquiring the frame on wgpu's surfaces).
//! The render thread releases the lock *fairly* after each frame
//! (`unlock_fair`), handing it straight to the window thread if that is
//! waiting: the window thread waits at most one frame of that card, does its
//! short work, and the card takes its lock back in time for its next
//! refresh. The window thread never waits for a card while the card could
//! be waiting for it: a card only takes the planner's lock (and the
//! playhead's), and those are never held while a card's lock is awaited.
//!
//! The window thread therefore takes **one card's lock at a time** wherever
//! it can (health, previews). Holding card A's lock while waiting for card
//! B's frame to end would stop A for up to a frame of B: A would miss a
//! refresh. Only a change that every card must make at once (a new show,
//! outputs rebuilt or closed) takes them all, and may cost a refresh.
//!
//! `LCPLAY_CARD_THREADS=0` drives every card from the window thread in turn,
//! as before, with the same code ([`Card::step`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::window::Window;

use crate::content::{self, Content};
use crate::frametime::{FrameTimer, Phase};
use crate::plan::{may_decide, FramePlan, Planner, RefreshLine, PREROLL_S};
use crate::playhead::{Playhead, Position};
use crate::render::{Gpu, OutputGpu, VirtualGpu};
use crate::show::{Output, Rect, Show, Source};
use crate::virtuals::{self, overlaps};

/// A card behind the lock its render thread and the window thread share.
pub type CardHandle = Arc<parking_lot::Mutex<Card>>;
/// A card's lock held by the window thread.
pub type CardGuard = parking_lot::ArcMutexGuard<parking_lot::RawMutex, Card>;

/// Whether each card gets its own render thread (`LCPLAY_CARD_THREADS`,
/// default on).
pub fn threads_enabled() -> bool {
    std::env::var("LCPLAY_CARD_THREADS").map_or(true, |v| v != "0")
}

/// What every card shares: the clock, the machine's frame plans, and the
/// flags the window thread reads.
pub struct Shared {
    pub playhead: Arc<Playhead>,
    pub planner: Planner,
    /// Render threads end.
    pub stop: AtomicBool,
    /// A card hit an error the Runner cannot continue after (GPU out of
    /// memory): the window thread exits, and the supervisor restarts it.
    pub fatal: AtomicBool,
}

/// The show as the cards render it.
pub struct Scene {
    pub show: Arc<Show>,
    /// Layer indices sorted back-to-front by z.
    pub draw_order: Vec<usize>,
    /// The planner's generation for this show ([`Planner::set_show`]).
    pub generation: u64,
}

impl Scene {
    pub fn new(show: Arc<Show>, generation: u64) -> Self {
        let layers = &show.layers;
        let mut draw_order: Vec<usize> = (0..layers.len()).collect();
        draw_order.sort_by(|&a, &b| {
            layers[a]
                .transform
                .z
                .partial_cmp(&layers[b].transform.z)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Scene {
            show,
            draw_order,
            generation,
        }
    }
}

/// An output rendered by a card.
pub struct CardOutput {
    /// Before `window`: fields drop in order, and the swap chain must be
    /// released before its window is destroyed.
    pub gpu: OutputGpu,
    pub window: Arc<Window>,
    /// The output as it was last applied.
    pub output: Output,
    /// Frames in which a layer on air on this output (directly, or through
    /// a virtual display it shows) showed a different frame than another
    /// card showed before it for the same refresh.
    pub split_frames: u64,
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

pub struct Card {
    /// Its place in the Runner's list of cards; card 0 reports the frame
    /// rate, and the lowest-numbered card with an output is the machine's
    /// timing reference ([`crate::plan`]).
    pub index: usize,
    pub gpu: Gpu,
    scene: Arc<Scene>,
    /// One entry per layer, parallel to `show.layers`…
    contents: Vec<Box<dyn Content>>,
    /// …and the identity of each, for reuse when the show is edited.
    content_keys: Vec<String>,
    /// One entry per virtual display, parallel to `show.virtuals`.
    virtuals: Vec<VirtualGpu>,
    /// The outputs this card renders, in the order of the show's displays.
    pub outputs: Vec<CardOutput>,
    /// Where each frame's time goes; slow frames are logged.
    pub timing: FrameTimer,
    stats: FrameStats,
}

impl Card {
    pub fn new(index: usize, gpu: Gpu) -> Self {
        Card {
            index,
            gpu,
            scene: Arc::new(Scene::new(Arc::new(Show::empty()), 0)),
            contents: Vec::new(),
            content_keys: Vec::new(),
            virtuals: Vec::new(),
            outputs: Vec::new(),
            timing: FrameTimer::from_env(),
            stats: FrameStats {
                frames: 0,
                since: Instant::now(),
            },
        }
    }

    /// How log lines name this card ("" for card 0, which was the only one
    /// before a Runner rendered on several).
    fn label(&self) -> String {
        if self.index == 0 {
            String::new()
        } else {
            format!("card {}: ", self.index)
        }
    }

    /// Render the show of `scene` from now on. Contents already loaded for
    /// the same source are kept (retimed), new ones are loaded, the rest
    /// dropped (for video, that stops its decoder).
    pub fn set_scene(&mut self, scene: Arc<Scene>) {
        let factory = self.gpu.factory();
        let show = &scene.show;

        // What is loaded now, by identity.
        let mut loaded: HashMap<String, Vec<Box<dyn Content>>> = HashMap::new();
        for (key, content) in self.content_keys.drain(..).zip(self.contents.drain(..)) {
            loaded.entry(key).or_default().push(content);
        }
        for layer in &show.layers {
            let key = content::cache_key(layer);
            let content = match loaded.get_mut(&key).and_then(Vec::pop) {
                Some(mut kept) => {
                    kept.retime(&layer.timing);
                    kept
                }
                None => {
                    tracing::debug!(layer = %layer.id, card = self.index, "loading content");
                    self.timing.note("content loaded");
                    content::build(layer, &factory).unwrap_or_else(|e| {
                        tracing::error!(layer = %layer.id, "{e:#}; showing placeholder");
                        content::placeholder(&factory)
                    })
                }
            };
            self.contents.push(content);
            self.content_keys.push(key);
        }
        drop(loaded);

        // Virtual displays: keep the picture of one whose id and resolution
        // did not change (a moved capture area needs nothing on the GPU).
        let layer_count = show.layers.len();
        let mut kept: HashMap<(String, (u32, u32)), VirtualGpu> = self
            .virtuals
            .drain(..)
            .map(|vg| ((vg.id.clone(), vg.size), vg))
            .collect();
        let gpu = &self.gpu;
        self.virtuals = show
            .virtuals
            .iter()
            .map(|v| {
                let mut vg = kept
                    .remove(&(v.id.clone(), (v.width, v.height)))
                    .unwrap_or_else(|| gpu.build_virtual(v, layer_count));
                gpu.set_virtual_layer_count(&mut vg, layer_count);
                vg
            })
            .collect();
        for out in &mut self.outputs {
            self.gpu.set_layer_count(&mut out.gpu, layer_count);
        }
        self.scene = scene;
    }

    /// Drop every loaded content, virtual display and output (the outputs
    /// are being rebuilt from scratch).
    pub fn clear(&mut self) {
        self.contents.clear();
        self.content_keys.clear();
        self.virtuals.clear();
        self.outputs.clear();
    }

    /// Open an output on this card, for `window`.
    pub fn add_output(
        &mut self,
        window: Arc<Window>,
        output: &Output,
        layer_count: usize,
    ) -> anyhow::Result<()> {
        let gpu = self.gpu.build_output(window.clone(), output, layer_count)?;
        tracing::info!(
            display = %output.id,
            gpu = %self.gpu.adapter_info(),
            card = self.index,
            presenter = gpu.presenter_name(),
            frame_latency = crate::render::frame_latency(),
            mode_hz = gpu
                .timing
                .clock
                .nominal()
                .map_or_else(|| "unknown".to_string(), |p| format!("{:.3}", 1e9 / p)),
            "output opened"
        );
        self.outputs.push(CardOutput {
            gpu,
            window,
            output: output.clone(),
            split_frames: 0,
        });
        Ok(())
    }

    /// The output showing display `id`.
    pub fn output_mut(&mut self, id: &str) -> Option<&mut CardOutput> {
        self.outputs.iter_mut().find(|o| o.output.id == id)
    }

    pub fn output(&self, id: &str) -> Option<&CardOutput> {
        self.outputs.iter().find(|o| o.output.id == id)
    }

    /// Stop rendering display `id` here (its swap chain is released).
    pub fn remove_output(&mut self, id: &str) -> Option<CardOutput> {
        let i = self.outputs.iter().position(|o| o.output.id == id)?;
        Some(self.outputs.remove(i))
    }

    /// Display `id`'s placement, warp, blend or rotation changed.
    pub fn update_output(&mut self, id: &str, output: &Output) {
        let Some(i) = self.outputs.iter().position(|o| o.output.id == id) else {
            return;
        };
        let out = &mut self.outputs[i];
        let geometry_changed = out.output.warp != output.warp
            || out.output.blend != output.blend
            || out.output.rotation_degrees != output.rotation_degrees;
        if geometry_changed {
            self.gpu.update_output(&mut out.gpu, output);
        }
        out.output = output.clone();
    }

    /// Display `id`'s window was resized.
    pub fn resize(&mut self, id: &str, width: u32, height: u32) {
        if let Some(i) = self.outputs.iter().position(|o| o.output.id == id) {
            self.gpu
                .resize_output(&mut self.outputs[i].gpu, width, height);
        }
    }

    /// Release every decoder (nothing of this card is on screen).
    pub fn idle(&mut self) {
        self.contents.iter_mut().for_each(|c| c.idle());
    }

    /// The shortest refresh period among the outputs, once measured.
    fn refresh_period(&self) -> Option<Duration> {
        self.outputs
            .iter()
            .filter_map(|o| o.gpu.timing.clock.period())
            .filter(|ns| ns.is_finite() && *ns > 0.0)
            .min_by(f64::total_cmp)
            .map(|ns| Duration::from_secs_f64(ns / 1e9))
    }

    /// One iteration of the card's loop: render a frame on every output,
    /// with the bookkeeping around it (frame timing, frame rate). Returns
    /// whether a frame was rendered — `false` when there is nothing to
    /// render on, and the caller should not spin.
    pub fn step(&mut self, shared: &Shared) -> bool {
        self.timing.begin();
        let Some(position) = self.render_frame(shared) else {
            self.timing.skip();
            return false;
        };
        if let Some(fps) = self.stats.tick() {
            if shared.planner.is_reference(self.index) {
                // The reference card's rate, reported to the Director with
                // the next health message.
                shared.playhead.report_fps(fps as f32);
            }
            let slow = self
                .timing
                .summary()
                .map_or_else(String::new, |s| format!("; {s}"));
            // Frames split between cards so far (only possible with
            // several): the measure of whether they show the same picture.
            let split: u64 = self.outputs.iter().map(|o| o.split_frames).sum();
            let split = if split > 0 {
                format!("; {split} split frame(s) so far")
            } else {
                String::new()
            };
            tracing::info!(
                "{}{fps:.1} fps (show time {:.1}s, frame {}){slow}{split}",
                self.label(),
                position.show_seconds(),
                position.frame
            );
        }
        let period = self.refresh_period();
        let show = &self.scene.show;
        if let Some(line) = self.timing.end(period, |i| layer_label(show, i)) {
            tracing::warn!("{}{line}", self.label());
        }
        true
    }

    /// Render the outputs of this card:
    ///
    /// 1. wait for each output to take a frame (this is where the loop is
    ///    paced by the displays);
    /// 2. work out the refresh the frame will appear on — the next one plus
    ///    the frames queued ahead of it, corrected by what the driver reports
    ///    — and get the machine's plan for it ([`Planner::plan`]);
    /// 3. update the contents of the layers this card shows, render, present.
    fn render_frame(&mut self, shared: &Shared) -> Option<Position> {
        if self.outputs.is_empty() {
            self.idle();
            shared.planner.set_line(self.index, None);
            return None;
        }
        let playhead = &shared.playhead;
        let local_now = || playhead.clock.local_now_ns();
        let mut frames = Vec::with_capacity(self.outputs.len());
        let acquiring = Instant::now();
        for w in 0..self.outputs.len() {
            match self.gpu.acquire(&mut self.outputs[w].gpu, local_now) {
                Ok(frame) => frames.push((w, frame)),
                Err(e) => self.surface_error(w, e, shared),
            }
        }
        self.timing.since(Phase::Acquire, acquiring);
        if frames.is_empty() {
            return None;
        }

        // The refresh each output's frame will reach, and the instant the
        // card's picture is for (that of its first output: outputs of one
        // card share one plan, which keeps them in lockstep).
        let now = local_now();
        let latency = crate::render::frame_latency() as i64;
        // (prediction without correction, correction, time of the refresh)
        let targets: Vec<Option<(i64, i64, i64)>> = frames
            .iter()
            .map(|(w, _)| {
                let t = &self.outputs[*w].gpu.timing;
                let (next, _) = t.clock.next_after(now)?;
                let base = next + latency - 1;
                let correction = t.tracker.correction();
                Some((base, correction, t.clock.time_of(base + correction)?))
            })
            .collect();
        let first = &self.outputs[frames[0].0].gpu.timing;
        let display_local = targets[0].map_or(now, |(_, _, at)| at);
        let own_period = first.clock.period();
        // The machine's timing reference, if this card owns it.
        let line =
            first
                .clock
                .last_refresh(now)
                .zip(own_period)
                .map(|((_, anchor_ns), period_ns)| RefreshLine {
                    anchor_ns,
                    period_ns,
                });
        shared.planner.set_line(self.index, line);
        let was_on_grid = first.grid_offset.is_some();
        let (plan, grid_offset) =
            shared
                .planner
                .plan(playhead, self.index, display_local, own_period, was_on_grid);
        // The planner's show and this card's change together: the window
        // thread changes them while holding every card's lock, and this card
        // holds its own while it renders. A plan for another show would not
        // match this card's contents layer for layer.
        if plan.generation() != self.scene.generation {
            tracing::error!(
                card = self.index,
                "frame plan for another show; frame skipped"
            );
            return None;
        }

        let scene = self.scene.clone();
        let show = &scene.show;
        let times = &plan.times;
        // The virtual displays these outputs show: those on air now are
        // rendered; the content of those on air or about to be is decoded
        // (so a cue in a virtual display starts on its first frame too).
        let areas: Vec<Rect> = self.outputs.iter().map(|o| o.output.canvas_rect).collect();
        let layers = &show.layers;
        let on_air = |i: usize| times[i].is_some_and(|t| layers[i].timing.visible_at(t));
        let soon = |i: usize| {
            times[i].is_some_and(|t| {
                let timing = &layers[i].timing;
                timing.visible_at(t) || (0.0..=PREROLL_S).contains(&(timing.start - t))
            })
        };
        let virtual_order = virtuals::plan(show, on_air, &areas);
        let virtual_reach = virtuals::shown_on(show, soon, &areas);
        let updating = Instant::now();
        let (late, split) = self.update_contents(show, &areas, &virtual_reach, &plan);
        self.timing.since(Phase::Update, updating);
        if !late.is_empty() {
            self.timing.note("late decode");
        }
        if !split.is_empty() {
            self.timing.note("split frame");
        }

        let views: Vec<_> = frames
            .iter()
            .map(|(w, f)| {
                let o = &self.outputs[*w];
                (&o.gpu, &o.output, &f.view)
            })
            .collect();
        let rendering = Instant::now();
        self.gpu.render(
            &views,
            show,
            times,
            &self.contents,
            &scene.draw_order,
            &self.virtuals,
            &virtual_order,
        );
        self.timing.since(Phase::Render, rendering);
        let presenting = Instant::now();
        for (w, _) in &frames {
            self.outputs[*w].window.pre_present_notify();
        }
        for ((w, frame), target) in frames.into_iter().zip(targets) {
            let o = &mut self.outputs[w];
            self.gpu
                .present(&mut o.gpu, frame, target.map(|(b, c, _)| (b, c)));
            o.gpu.timing.grid_offset = grid_offset;
            if late.contains(&w) {
                o.gpu.timing.late_decodes += 1;
            }
            if split.contains(&w) {
                o.split_frames += 1;
            }
        }
        self.timing.since(Phase::Present, presenting);
        Some(plan.position)
    }

    /// Bring this card's contents to `plan`. Only layers that fall on one
    /// of the card's outputs (`areas`, parallel to `self.outputs`), or in
    /// the area of a virtual display those outputs show (`virtual_reach`:
    /// each such area with the outputs it reaches), are decoded; the others
    /// are idle on this card.
    ///
    /// A video layer shows the frame decided for the plan if another card
    /// has already decided one ([`FramePlan::decide`]); otherwise this card
    /// updates it, and decides if the frame it shows is close enough to the
    /// one due ([`may_decide`]). Returns the outputs on which a video layer
    /// on air was late, and those on which one showed another frame than a
    /// card before it did for the same plan.
    fn update_contents(
        &mut self,
        show: &Show,
        areas: &[Rect],
        virtual_reach: &[(Rect, Vec<usize>)],
        plan: &FramePlan,
    ) -> (Vec<usize>, Vec<usize>) {
        let factory = self.gpu.factory();
        let mut late = Vec::new();
        let mut split = Vec::new();
        // Each layer's update is timed from its start to the next one's (the
        // loop body has early `continue`s).
        let mut started: Option<(usize, Instant)> = None;
        for (i, (layer, c)) in show.layers.iter().zip(&mut self.contents).enumerate() {
            if let Some((j, at)) = started.replace((i, Instant::now())) {
                self.timing.layer(j, at.elapsed());
            }
            let Some(step) = plan.steps.get(i) else {
                c.idle();
                continue;
            };
            let Some(media) = step.media else {
                c.idle();
                continue;
            };
            // The outputs it appears on: directly, or inside a virtual
            // display that they show.
            let mut here: Vec<usize> = areas
                .iter()
                .enumerate()
                .filter(|(_, r)| overlaps(&layer.transform, r))
                .map(|(w, _)| w)
                .collect();
            for (r, outputs) in virtual_reach {
                if overlaps(&layer.transform, r) {
                    here.extend(outputs);
                }
            }
            // None: another card (or Runner) shows it. Decoding it here
            // would only cost time.
            if here.is_empty() {
                c.idle();
                continue;
            }
            let decided = plan.decided(i);
            match decided {
                Some(frame) => c.follow(media, frame, &factory),
                None => c.update(media, &factory),
            }
            if let Some(shown) = c.shown_frame() {
                // A stale picture (the decoder restarting, or held back from
                // restarting) is not imposed on the other cards: they show
                // their own, and the next of them to update this layer
                // decides.
                if decided.is_none() && c.due_frame().is_some_and(|due| may_decide(shown, due)) {
                    plan.decide(i, shown);
                }
                // Before it is on air, a frame shown is only preparation.
                if plan.shown(i, shown) && step.on_air {
                    split.extend(&here);
                }
            }
            if step.on_air && c.late() {
                late.extend(&here);
            }
        }
        if let Some((j, at)) = started {
            self.timing.layer(j, at.elapsed());
        }
        late.sort_unstable();
        late.dedup();
        split.sort_unstable();
        split.dedup();
        (late, split)
    }

    /// React to an output whose frame could not be acquired.
    fn surface_error(&mut self, w: usize, err: wgpu::SurfaceError, shared: &Shared) {
        let o = &mut self.outputs[w];
        match err {
            wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                let size = o.window.inner_size();
                self.gpu.resize_output(&mut o.gpu, size.width, size.height);
            }
            wgpu::SurfaceError::OutOfMemory => {
                tracing::error!("GPU out of memory; exiting");
                // Not a close: the supervisor restarts it.
                shared.fatal.store(true, Ordering::Relaxed);
            }
            // Typical for a minimized/occluded window; it would otherwise
            // log at the frame rate.
            wgpu::SurfaceError::Timeout => {
                tracing::debug!(output = %o.output.id, "surface timeout")
            }
            e => tracing::warn!("surface error: {e:?}"),
        }
    }

    /// Display `id`'s presentation health.
    pub fn health(&self, id: &str) -> Option<lc_proto::OutputHealth> {
        let o = self.output(id)?;
        let t = &o.gpu.timing;
        let exact = o.gpu.exact_timing();
        Some(lc_proto::OutputHealth {
            id: id.to_string(),
            gpu: self.gpu.adapter_info(),
            presenter: o.gpu.presenter_name().into(),
            refresh_hz: t.clock.period().map(|p| 1e9 / p),
            presents: t.tracker.presents,
            missed: t.tracker.missed,
            late_decodes: t.late_decodes,
            split_frames: o.split_frames,
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
        })
    }
}

/// Start `card`'s render thread. It runs until `shared.stop` is set.
pub fn spawn(
    card: CardHandle,
    shared: Arc<Shared>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let index = card.lock().index;
    std::thread::Builder::new()
        .name(format!("lc-render-{index}"))
        .spawn(move || {
            crate::priority::render_thread();
            while !shared.stop.load(Ordering::Relaxed) {
                let mut guard = card.lock();
                let rendered = guard.step(&shared);
                // Hand the lock to the window thread if it is waiting.
                parking_lot::MutexGuard::unlock_fair(guard);
                if !rendered {
                    // Nothing to render on (no output yet, or none took a
                    // frame): look again shortly instead of spinning.
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        })
}

/// How the frame timing log names layer `index`: its id, and the file it
/// shows, if any.
pub fn layer_label(show: &Show, index: usize) -> String {
    let Some(layer) = show.layers.get(index) else {
        return format!("layer #{index}");
    };
    match &layer.source {
        Source::Video { path } | Source::Image { path } => {
            let file = std::path::Path::new(path)
                .file_name()
                .map_or_else(|| path.clone(), |f| f.to_string_lossy().into_owned());
            format!("'{}' ({file})", layer.id)
        }
        _ => format!("'{}'", layer.id),
    }
}
