//! The machine's frame plan: what every layer shows on the refresh a frame
//! is rendered for, decided **once per machine**, whichever card asks.
//!
//! ## Why
//!
//! A Runner renders each output on the card its monitor is connected to,
//! and since several cards can share a layer (a cue spanning two cards'
//! outputs), every card must put the *same* frame of that layer on screen
//! for the same refresh. Otherwise the picture is split at the boundary
//! between the cards. Each card used to sample the show clock for itself, at
//! its own instant, with its own decoders; two cards could then land on
//! either side of a frame boundary, or one card's decoder could be late
//! while the other's was not.
//!
//! ## How
//!
//! A frame is rendered for a refresh (see [`crate::vsync`]). That refresh is
//! mapped to a **plan key** that every card computes the same way:
//!
//! * on the Director's refresh grid (frame-locked Runners): the grid
//!   instant, an integer that is the same on every machine;
//! * otherwise, the nearest refresh of the machine's **reference output**
//!   (the first output of the lowest-numbered card that has one), when the
//!   card's refreshes run at the same rate. With frame lock the cards'
//!   refreshes coincide and this is exact; without it, a card's refresh is
//!   up to half a period from the reference one it is mapped to — the
//!   scanouts themselves are not aligned, which only frame lock can fix;
//! * a card whose refresh rate differs from the reference's plans for
//!   itself (nothing to share).
//!
//! The first card to need a key builds its [`FramePlan`] — the position on
//! the show clock, each layer's time, which layers are updated and to what
//! media time, free-run on-air instants — and the others reuse it. A plan is
//! built from one clock reading, so the cards cannot disagree about it.
//!
//! ## One decision per frame when a decoder is late
//!
//! A plan also records, per layer, the media frame the first card to update
//! that layer actually showed ([`FramePlan::decide`]). Cards updating the
//! layer later for the same plan show that frame
//! ([`crate::content::Content::follow`]) instead of deciding again: if the
//! first card's decoder was late, every card holds the same older frame,
//! rather than one card showing frame N and the other N−1. When a follower
//! cannot show the decided frame (its own decoder is behind), that is
//! counted per output as a *split frame*.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use lc_proto::NANOS_PER_SEC;

use crate::app::on_air_since;
use crate::playhead::{Playhead, Position};
use crate::show::Show;

/// How long before a layer appears its content starts preparing (seconds).
pub const PREROLL_S: f64 = 1.0;

/// Plans kept for cards that render the same refresh later.
const RECENT_PLANS: usize = 8;

/// How far two refresh periods may differ (relative) for one card's
/// refreshes to be mapped onto another's: 500 ppm. Independent displays at
/// the same nominal rate differ by tens of ppm; 59.94 Hz against 60 Hz is
/// 1000 ppm and must not match (one display would periodically show a frame
/// twice to follow the other).
const SAME_RATE: f64 = 500e-6;

/// What one layer does in a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerStep {
    /// Visible at this frame's time.
    pub on_air: bool,
    /// The media time to bring the content to — `None`: the layer is off
    /// stage and not about to appear, and its content goes idle.
    pub media: Option<f64>,
}

/// How a plan is identified across cards.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Key {
    /// On the Director's refresh grid: the grid instant (cluster ns).
    Grid(i64),
    /// On the reference output's refreshes: the reference refresh's local
    /// time, ns (matched within a quarter period: the reference's line is
    /// refitted as it goes, so two cards can compute it a few ns apart).
    Machine { local_ns: i64, period_ns: f64 },
    /// Not shared.
    Own,
}

/// Everything a frame needs from the show clock, the same for every card
/// that renders the same refresh.
#[derive(Debug)]
pub struct FramePlan {
    key: Key,
    /// The show generation it was built for ([`Planner::set_show`]).
    generation: u64,
    pub position: Position,
    /// Each layer's time on its timeline (`None`: that timeline is off
    /// stage), parallel to `show.layers`.
    pub times: Vec<Option<f64>>,
    /// Parallel to `show.layers`.
    pub steps: Vec<LayerStep>,
    /// Per layer: the media frame the first card to update it showed.
    decisions: Mutex<Vec<Option<i64>>>,
}

impl FramePlan {
    /// The frame decided for `layer`, if a card has shown it already.
    pub fn decided(&self, layer: usize) -> Option<i64> {
        self.decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(layer)
            .copied()
            .flatten()
    }

    /// Record that `frame` of `layer` was shown — unless another card got
    /// there first, whose frame is returned (and stands).
    pub fn decide(&self, layer: usize, frame: i64) -> i64 {
        let mut decisions = self.decisions.lock().unwrap_or_else(|e| e.into_inner());
        match decisions.get_mut(layer) {
            Some(slot) => *slot.get_or_insert(frame),
            None => frame,
        }
    }

    /// Whether other cards can be given this plan.
    pub fn is_shared(&self) -> bool {
        self.key != Key::Own
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// A card's refresh line, `t(n) = anchor + n × period` in local ns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RefreshLine {
    pub anchor_ns: i64,
    pub period_ns: f64,
}

/// The refresh of `line` nearest to `local_ns`, if a display refreshing
/// every `own_period_ns` can be mapped onto it (same rate within
/// [`SAME_RATE`]).
pub fn snap_to(line: RefreshLine, local_ns: i64, own_period_ns: Option<f64>) -> Option<i64> {
    let period = line.period_ns;
    if !(period.is_finite() && period > 0.0) {
        return None;
    }
    let own = own_period_ns?;
    if ((own - period) / period).abs() > SAME_RATE {
        return None;
    }
    let n = ((local_ns - line.anchor_ns) as f64 / period).round();
    Some(line.anchor_ns + (n * period).round() as i64)
}

/// Each layer's time: that of the timeline it belongs to — the main
/// timeline's, or its task's (`None` while that task is off stage).
pub fn layer_times(
    show: &Show,
    position: &Position,
    task_seconds: impl Fn(&str, i64) -> Option<f64>,
) -> Vec<Option<f64>> {
    let now = position.show_seconds();
    let mut task_times: HashMap<&str, Option<f64>> = HashMap::new();
    show.layers
        .iter()
        .map(|layer| match layer.timeline.as_deref() {
            // Test patterns run on the cluster clock, whatever the transport
            // does.
            _ if layer.cluster_clock => {
                Some(position.cluster_ns.max(0) as f64 / NANOS_PER_SEC as f64)
            }
            None => Some(now),
            Some(id) => *task_times
                .entry(id)
                .or_insert_with(|| task_seconds(id, position.cluster_ns)),
        })
        .collect()
}

/// What each layer does in the frame at `position`, and the bookkeeping of
/// free-running layers: `onsets` maps a free-running layer on air to the
/// cluster time it went on air (its media runs from there).
///
/// `task_state` gives an auxiliary timeline's transport at a cluster time.
pub fn layer_steps(
    show: &Show,
    position: &Position,
    times: &[Option<f64>],
    onsets: &mut HashMap<String, i64>,
    task_state: impl Fn(&str, i64) -> Option<lc_proto::TransportState>,
) -> Vec<LayerStep> {
    let loop_s = position
        .transport
        .loop_ns
        .map(|ns| ns as f64 / NANOS_PER_SEC as f64);
    show.layers
        .iter()
        .zip(times)
        .map(|(layer, time)| {
            let Some(t) = *time else {
                // Its timeline is off stage. Played again later, it goes on
                // air afresh.
                onsets.remove(&layer.id);
                return LayerStep {
                    on_air: false,
                    media: None,
                };
            };
            // A free-running layer is asked for media time: seconds since it
            // went on air, at normal speed, whatever its timeline does.
            let on_air = layer.timing.visible_at(t);
            if layer.timing.free_run {
                if on_air {
                    if !onsets.contains_key(&layer.id) {
                        let state = match layer.timeline.as_deref() {
                            None => Some(position.transport),
                            Some(id) => task_state(id, position.cluster_ns),
                        };
                        let onset = state.map_or(position.cluster_ns, |s| {
                            on_air_since(&s, layer.timing.start, position.cluster_ns)
                        });
                        onsets.insert(layer.id.clone(), onset);
                    }
                } else {
                    onsets.remove(&layer.id);
                }
            } else {
                // Free run switched off: switched back on, it starts afresh.
                onsets.remove(&layer.id);
            }
            let onset = onsets.get(&layer.id).copied();
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
            let media = if on_air {
                Some(media(t))
            } else if (0.0..=PREROLL_S).contains(&until_start) {
                // About to appear: get its first frame decoded and into the
                // texture now, so the cue starts on its first frame instead
                // of on whatever the decoder manages a moment later.
                Some(media(layer.timing.start))
            } else {
                // Off screen: an invisible video cue must not keep a decoder
                // (an ffmpeg process and several frames of memory) around.
                None
            };
            LayerStep { on_air, media }
        })
        .collect()
}

/// Builds and shares the machine's frame plans. One per Runner, shared by
/// every card's render loop.
pub struct Planner {
    inner: Mutex<Inner>,
}

struct Inner {
    show: Arc<Show>,
    generation: u64,
    /// Free-running layers on air: layer id → the cluster time they went on
    /// air.
    onsets: HashMap<String, i64>,
    /// The latest plans, newest last.
    recent: VecDeque<Arc<FramePlan>>,
    /// Per card: the refresh line of its first output.
    lines: Vec<Option<RefreshLine>>,
}

impl Inner {
    /// The reference output's line: the lowest-numbered card's.
    fn reference(&self) -> Option<RefreshLine> {
        self.lines.iter().flatten().next().copied()
    }

    fn find(&self, key: Key) -> Option<Arc<FramePlan>> {
        self.recent
            .iter()
            .rev()
            .find(|p| {
                p.generation == self.generation
                    && match (key, p.key) {
                        (Key::Grid(a), Key::Grid(b)) => a == b,
                        (
                            Key::Machine {
                                local_ns: a,
                                period_ns,
                            },
                            Key::Machine { local_ns: b, .. },
                        ) => ((a - b).abs() as f64) < period_ns / 4.0,
                        _ => false,
                    }
            })
            .cloned()
    }

    fn build(&mut self, key: Key, position: Position, playhead: &Playhead) -> Arc<FramePlan> {
        let show = self.show.clone();
        let times = layer_times(&show, &position, |id, at| playhead.task_seconds(id, at));
        let steps = layer_steps(&show, &position, &times, &mut self.onsets, |id, at| {
            playhead.task_state(id, at)
        });
        let plan = Arc::new(FramePlan {
            key,
            generation: self.generation,
            position,
            times,
            decisions: Mutex::new(vec![None; steps.len()]),
            steps,
        });
        if plan.is_shared() {
            if self.recent.len() >= RECENT_PLANS {
                self.recent.pop_front();
            }
            self.recent.push_back(plan.clone());
        }
        plan
    }
}

impl Default for Planner {
    fn default() -> Self {
        Self::new(Arc::new(Show::empty()))
    }
}

impl Planner {
    pub fn new(show: Arc<Show>) -> Self {
        Planner {
            inner: Mutex::new(Inner {
                show,
                generation: 0,
                onsets: HashMap::new(),
                recent: VecDeque::new(),
                lines: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make `show` the one plans are built for. Plans of the previous show
    /// are no longer handed out; on-air instants of free-running layers that
    /// are still there are kept. Returns the new generation.
    pub fn set_show(&self, show: Arc<Show>) -> u64 {
        let mut inner = self.lock();
        inner
            .onsets
            .retain(|id, _| show.layers.iter().any(|l| &l.id == id && l.timing.free_run));
        inner.show = show;
        inner.generation += 1;
        inner.recent.clear();
        inner.generation
    }

    /// The show generation plans are built for.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Card `card`'s first output refreshes on `line` (`None`: it has no
    /// output, or no estimate yet).
    pub fn set_line(&self, card: usize, line: Option<RefreshLine>) {
        let mut inner = self.lock();
        if inner.lines.len() <= card {
            inner.lines.resize(card + 1, None);
        }
        inner.lines[card] = line;
    }

    /// The plan for a frame card `card` renders for the refresh at local
    /// time `display_local_ns`. `own_period_ns` is that output's refresh
    /// period; `was_on_grid` whether it was on the Director's grid last
    /// frame. Also returns the distance from the Director's grid (`None`: not
    /// on it), for the output's health.
    pub fn plan(
        &self,
        playhead: &Playhead,
        card: usize,
        display_local_ns: i64,
        own_period_ns: Option<f64>,
        was_on_grid: bool,
    ) -> (Arc<FramePlan>, Option<i64>) {
        // Clock readings outside the lock: other cards plan meanwhile.
        let (position, grid_offset) = playhead.position_for_display(display_local_ns, was_on_grid);
        if grid_offset.is_some() {
            let key = Key::Grid(position.cluster_ns);
            let mut inner = self.lock();
            let plan = match inner.find(key) {
                Some(plan) => plan,
                None => inner.build(key, position, playhead),
            };
            return (plan, grid_offset);
        }

        let line = self.lock().reference();
        let snapped = line.and_then(|l| {
            let own = if self.is_reference(card) {
                Some(l.period_ns)
            } else {
                own_period_ns
            };
            snap_to(l, display_local_ns, own).map(|at| (at, l.period_ns))
        });
        let Some((local_ns, period_ns)) = snapped else {
            let mut inner = self.lock();
            return (inner.build(Key::Own, position, playhead), None);
        };
        let key = Key::Machine {
            local_ns,
            period_ns,
        };
        if let Some(plan) = self.lock().find(key) {
            return (plan, None);
        }
        let position = playhead.position_at_local(local_ns);
        let mut inner = self.lock();
        // Another card may have built it meanwhile.
        let plan = match inner.find(key) {
            Some(plan) => plan,
            None => inner.build(key, position, playhead),
        };
        (plan, None)
    }

    /// Whether `card` owns the reference output.
    pub fn is_reference(&self, card: usize) -> bool {
        let inner = self.lock();
        inner.lines.iter().position(Option::is_some) == Some(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::show::Layer;
    use lc_proto::TransportState;

    const S: i64 = NANOS_PER_SEC;

    fn layer(id: &str, start: f64, free_run: bool) -> Layer {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "source": { "type": "color", "rgba": [1.0, 1.0, 1.0, 1.0] },
            "transform": { "position": [0.0, 0.0], "size": [10.0, 10.0] },
            "timing": { "start": start, "end": start + 20.0, "free_run": free_run },
        }))
        .unwrap()
    }

    fn position(cluster_ns: i64, show_ns: i64, playing: bool) -> Position {
        Position {
            cluster_ns,
            show_ns,
            frame: 0,
            transport: TransportState {
                playing,
                anchor_cluster_ns: cluster_ns - show_ns,
                anchor_show_ns: 0,
                rate_permille: 1000,
                ..Default::default()
            },
        }
    }

    #[test]
    fn snapping_maps_a_refresh_onto_the_nearest_reference_refresh() {
        let line = RefreshLine {
            anchor_ns: 1_000_000,
            period_ns: 16_666_667.0,
        };
        let p = Some(16_666_667.0);
        // Exactly on a refresh, and within half a period either side.
        assert_eq!(snap_to(line, 1_000_000, p), Some(1_000_000));
        assert_eq!(snap_to(line, 1_000_000 + 8_000_000, p), Some(1_000_000));
        assert_eq!(
            snap_to(line, 1_000_000 + 9_000_000, p),
            Some(1_000_000 + 16_666_667)
        );
        assert_eq!(
            snap_to(line, 1_000_000 - 9_000_000, p),
            Some(1_000_000 - 16_666_667)
        );
        // Same nominal rate, 50 ppm apart: mapped. 59.94 Hz on 60 Hz: not.
        assert!(snap_to(line, 5_000_000, Some(16_666_667.0 * (1.0 + 50e-6))).is_some());
        assert!(snap_to(line, 5_000_000, Some(1e9 / 59.94)).is_none());
        // No rate known for the card: not mapped.
        assert!(snap_to(line, 5_000_000, None).is_none());
    }

    #[test]
    fn steps_follow_cues_on_air_preroll_and_off_stage() {
        let show = Show {
            layers: vec![
                layer("now", 0.0, false),
                layer("soon", 10.5, false),
                layer("later", 30.0, false),
            ],
            ..Show::empty()
        };
        let pos = position(100 * S, 10 * S, true);
        let times = layer_times(&show, &pos, |_, _| None);
        assert_eq!(times, vec![Some(10.0); 3]);
        let steps = layer_steps(&show, &pos, &times, &mut HashMap::new(), |_, _| None);
        assert_eq!(
            steps,
            vec![
                LayerStep {
                    on_air: true,
                    media: Some(10.0)
                },
                // Half a second before it appears: its first frame.
                LayerStep {
                    on_air: false,
                    media: Some(10.5)
                },
                LayerStep {
                    on_air: false,
                    media: None
                },
            ]
        );
    }

    #[test]
    fn a_free_running_layer_runs_from_when_it_went_on_air() {
        let show = Show {
            layers: vec![layer("free", 2.0, true)],
            ..Show::empty()
        };
        let mut onsets = HashMap::new();
        // Playing through its start at cluster 100 s + 2 s; seen at 103 s.
        let pos = position(103 * S, 3 * S, true);
        let times = layer_times(&show, &pos, |_, _| None);
        let steps = layer_steps(&show, &pos, &times, &mut onsets, |_, _| None);
        assert_eq!(onsets.get("free"), Some(&(102 * S)));
        assert_eq!(steps[0].media, Some(1.0));
        // Off air: forgotten.
        let pos = position(130 * S, 30 * S, true);
        let times = layer_times(&show, &pos, |_, _| None);
        layer_steps(&show, &pos, &times, &mut onsets, |_, _| None);
        assert!(onsets.is_empty());
    }

    #[test]
    fn the_first_card_decides_and_the_decision_stands() {
        let plan = FramePlan {
            key: Key::Grid(0),
            generation: 0,
            position: position(0, 0, false),
            times: vec![Some(0.0); 2],
            steps: vec![
                LayerStep {
                    on_air: true,
                    media: Some(0.0)
                };
                2
            ],
            decisions: Mutex::new(vec![None; 2]),
        };
        assert_eq!(plan.decided(0), None);
        assert_eq!(plan.decide(0, 41), 41);
        assert_eq!(plan.decide(0, 42), 41);
        assert_eq!(plan.decided(0), Some(41));
        assert_eq!(plan.decided(1), None);
        // Out of range (a show change raced it): nothing recorded.
        assert_eq!(plan.decide(5, 7), 7);
    }

    #[test]
    fn plans_are_shared_by_refresh_and_dropped_with_the_show() {
        let planner = Planner::default();
        let mut inner = planner.lock();
        let playhead_free_plan = |inner: &mut Inner, key| {
            let plan = Arc::new(FramePlan {
                key,
                generation: inner.generation,
                position: position(0, 0, false),
                times: Vec::new(),
                steps: Vec::new(),
                decisions: Mutex::new(Vec::new()),
            });
            inner.recent.push_back(plan.clone());
            plan
        };
        let period_ns = 16_666_667.0;
        let a = playhead_free_plan(
            &mut inner,
            Key::Machine {
                local_ns: 1_000_000,
                period_ns,
            },
        );
        // A few ns apart (refitted line): the same refresh.
        let found = inner
            .find(Key::Machine {
                local_ns: 1_000_040,
                period_ns,
            })
            .unwrap();
        assert!(Arc::ptr_eq(&a, &found));
        // The next refresh: not.
        assert!(inner
            .find(Key::Machine {
                local_ns: 1_000_000 + 16_666_667,
                period_ns
            })
            .is_none());
        let g = playhead_free_plan(&mut inner, Key::Grid(5));
        assert!(Arc::ptr_eq(&g, &inner.find(Key::Grid(5)).unwrap()));
        assert!(inner.find(Key::Grid(6)).is_none());
        drop(inner);
        planner.set_show(Arc::new(Show::empty()));
        assert!(planner.lock().find(Key::Grid(5)).is_none());
    }

    #[test]
    fn two_cards_on_the_same_refresh_get_the_same_plan() {
        let clock = Arc::new(lc_time::ClusterClock::new(Arc::new(
            lc_time::MonotonicClock::new(),
        )));
        let playing = TransportState {
            playing: true,
            rate_permille: 1000,
            ..TransportState::STOPPED
        };
        let playhead = Playhead::new(clock, playing, lc_proto::FrameRate::default());
        let show = Arc::new(Show {
            layers: vec![layer("a", 0.0, false)],
            ..Show::empty()
        });
        let planner = Planner::new(show);
        let period = 16_666_667.0;
        let anchor = 1_000_000_000;
        // Card 0 owns the reference output; card 1's display is 3 ms behind
        // it (not frame-locked), at the same rate.
        planner.set_line(
            0,
            Some(RefreshLine {
                anchor_ns: anchor,
                period_ns: period,
            }),
        );
        let refresh = anchor + 10 * period as i64;
        let (a, _) = planner.plan(&playhead, 0, refresh, Some(period), false);
        let (b, _) = planner.plan(&playhead, 1, refresh + 3_000_000, Some(period), false);
        assert!(a.is_shared());
        assert!(Arc::ptr_eq(&a, &b), "one plan for both cards");
        // The next refresh is another plan.
        let (c, _) = planner.plan(
            &playhead,
            1,
            refresh + 3_000_000 + period as i64,
            Some(period),
            false,
        );
        assert!(!Arc::ptr_eq(&a, &c));
        assert!(c.position.cluster_ns > a.position.cluster_ns);
        // A card at another refresh rate plans for itself.
        let (d, _) = planner.plan(&playhead, 1, refresh, Some(1e9 / 50.0), false);
        assert!(!d.is_shared());
        assert!(!Arc::ptr_eq(&a, &d));
    }

    #[test]
    fn the_reference_is_the_lowest_card_with_a_line() {
        let planner = Planner::default();
        let line = |a| {
            Some(RefreshLine {
                anchor_ns: a,
                period_ns: 1.0e7,
            })
        };
        planner.set_line(1, line(7));
        assert!(planner.is_reference(1));
        planner.set_line(0, line(3));
        assert!(planner.is_reference(0));
        assert_eq!(planner.lock().reference().map(|l| l.anchor_ns), Some(3));
        planner.set_line(0, None);
        assert!(planner.is_reference(1));
    }
}
