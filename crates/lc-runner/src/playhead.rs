//! The **playhead**: "which show time, which frame, right now?"
//!
//! This is the one object shared between the two halves of a Runner:
//!
//! * the **agent** (network side) writes to it — clock samples, transport
//!   states and the show arriving from the Director;
//! * the **renderer** (GPU side) reads from it once per frame.
//!
//! Reading never blocks on the network and never fails. If the Director
//! disappears, the playhead keeps answering from the last transport state and
//! the clock's drift estimate (holdover) — which is the whole resilience
//! story of a Runner, in one place.
//!
//! A stand-alone Runner (no Director at all) uses the very same object: its
//! cluster clock is then simply its local clock, and transport changes come
//! from the keyboard via [`Playhead::apply_local`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use lc_proto::{ClockId, FrameRate, TransportState, NANOS_PER_SEC};
use lc_time::{next_state, ClusterClock, TransportChange, TransportTimeline};

/// A show document as distributed by the Director.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShowDoc {
    pub revision: u64,
    pub document: serde_json::Value,
}

/// Where the show is at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub cluster_ns: i64,
    pub show_ns: i64,
    pub frame: i64,
    /// The transport state that produced it.
    pub transport: TransportState,
}

impl Position {
    pub fn show_seconds(&self) -> f64 {
        self.show_ns as f64 / NANOS_PER_SEC as f64
    }
}

pub struct Playhead {
    pub clock: Arc<ClusterClock>,
    inner: Mutex<Inner>,
    /// True while a control connection to a Director is up.
    connected: AtomicBool,
    /// Presentation rate measured by the renderer (f32 bits; 0 = unknown).
    fps_bits: AtomicU32,
}

struct Inner {
    timeline: TransportTimeline,
    /// Timebase the timeline's anchors are expressed in. `None` = local time
    /// (stand-alone, or restored from cache before any Director was seen).
    timeline_clock: Option<ClockId>,
    frame_rate: FrameRate,
    show: Option<Arc<ShowDoc>>,
    /// Last show time handed out; held while clock and timeline disagree.
    last_show_ns: i64,
}

impl Playhead {
    pub fn new(clock: Arc<ClusterClock>, initial: TransportState, frame_rate: FrameRate) -> Self {
        Playhead {
            clock,
            inner: Mutex::new(Inner {
                timeline: TransportTimeline::new(initial),
                timeline_clock: None,
                frame_rate,
                show: None,
                last_show_ns: initial.anchor_show_ns,
            }),
            connected: AtomicBool::new(false),
            fps_bits: AtomicU32::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A render loop must survive a panic on some other thread.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The position right now.
    pub fn position(&self) -> Position {
        self.position_at_local(self.clock.local_now_ns())
    }

    /// The position at a given *local* time — pass the predicted time of the
    /// next vertical blank to render the frame that will be due when it is
    /// actually displayed.
    pub fn position_at_local(&self, local_ns: i64) -> Position {
        let reading = self.clock.read(local_ns);
        let mut inner = self.lock();
        let transport = inner.timeline.state_at(reading.cluster_ns);

        // The timeline's anchors only mean something in the timebase they
        // were written in. While the Runner is switching to a different
        // Director, there is a brief moment where the clock already follows
        // the new timebase but has not measured it yet (or the new transport
        // has not been installed yet). Evaluating across that mismatch would
        // flash an arbitrary frame, so the picture is held instead.
        let coherent = reading.clock_id == inner.timeline_clock
            && (reading.synced || reading.clock_id.is_none());
        let show_ns = if coherent {
            transport.show_time_at(reading.cluster_ns)
        } else {
            inner.last_show_ns
        };
        inner.last_show_ns = show_ns;

        Position {
            cluster_ns: reading.cluster_ns,
            show_ns,
            frame: inner.frame_rate.frame_at(show_ns),
            transport,
        }
    }

    /// Newest transport state known (in force or still pending).
    pub fn transport(&self) -> TransportState {
        self.lock().timeline.latest()
    }

    /// Timebase the current transport belongs to.
    pub fn timeline_clock(&self) -> Option<ClockId> {
        self.lock().timeline_clock
    }

    pub fn frame_rate(&self) -> FrameRate {
        self.lock().frame_rate
    }

    pub fn set_frame_rate(&self, frame_rate: FrameRate) {
        self.lock().frame_rate = frame_rate;
    }

    /// A transport state from the Director, expressed in timebase `clock_id`.
    ///
    /// Same timebase as before: queued, and takes over at its anchor (stale
    /// epochs are ignored). Different timebase: replaces everything.
    /// Returns whether the state was accepted.
    pub fn accept(&self, clock_id: ClockId, state: TransportState) -> bool {
        let mut inner = self.lock();
        if inner.timeline_clock == Some(clock_id) {
            inner.timeline.push(state)
        } else {
            inner.timeline.replace(state);
            inner.timeline_clock = Some(clock_id);
            true
        }
    }

    /// A local operator action (keyboard on a stand-alone Runner): takes
    /// effect immediately, no lead time needed with a single node.
    pub fn apply_local(&self, change: TransportChange) {
        let now = self.clock.now_ns();
        let mut inner = self.lock();
        let next = next_state(&inner.timeline.state_at(now), change, now);
        // `next_state` numbers from the state in force; make sure it also
        // outranks anything pending.
        let next = TransportState {
            epoch: inner.timeline.latest().epoch + 1,
            ..next
        };
        inner.timeline.push(next);
        inner.timeline.prune(now);
    }

    /// Drop transport states that can no longer apply. Housekeeping.
    pub fn prune(&self) {
        let now = self.clock.now_ns();
        self.lock().timeline.prune(now);
    }

    pub fn show(&self) -> Option<Arc<ShowDoc>> {
        self.lock().show.clone()
    }

    pub fn set_show(&self, show: ShowDoc) {
        self.lock().show = Some(Arc::new(show));
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Whether this Runner is (or has been) under a Director's control. Local
    /// transport keys are disabled then: one node changing the transport on
    /// its own would break sync with the others.
    pub fn is_clustered(&self) -> bool {
        self.timeline_clock().is_some()
    }

    /// Called by the renderer with its measured frame rate.
    pub fn report_fps(&self, fps: f32) {
        self.fps_bits.store(fps.to_bits(), Ordering::Relaxed);
    }

    pub fn fps(&self) -> Option<f32> {
        match f32::from_bits(self.fps_bits.load(Ordering::Relaxed)) {
            fps if fps > 0.0 => Some(fps),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lc_time::LocalClock;
    use std::sync::atomic::AtomicI64;

    struct Manual(AtomicI64);
    impl LocalClock for Manual {
        fn now_ns(&self) -> i64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    const S: i64 = NANOS_PER_SEC;

    fn playhead() -> (Arc<Manual>, Playhead) {
        let local = Arc::new(Manual(AtomicI64::new(0)));
        let clock = Arc::new(ClusterClock::new(local.clone()));
        (
            local,
            Playhead::new(clock, TransportState::STOPPED, FrameRate::FPS_60),
        )
    }

    #[test]
    fn standalone_playhead_follows_local_keys() {
        let (local, ph) = playhead();
        assert!(!ph.is_clustered());
        ph.apply_local(TransportChange::Play);
        local.0.store(2 * S, Ordering::Relaxed);
        let pos = ph.position();
        assert_eq!((pos.show_ns, pos.frame), (2 * S, 120));

        ph.apply_local(TransportChange::SeekBy(5 * S));
        assert_eq!(ph.position().show_ns, 7 * S);
        ph.apply_local(TransportChange::Pause);
        local.0.store(10 * S, Ordering::Relaxed);
        assert_eq!(ph.position().show_ns, 7 * S);
    }

    #[test]
    fn picture_is_held_while_switching_timebase() {
        let (local, ph) = playhead();
        ph.apply_local(TransportChange::Play);
        local.0.store(3 * S, Ordering::Relaxed);
        assert_eq!(ph.position().show_ns, 3 * S);

        // A Director appears: the clock starts following its timebase…
        ph.clock.follow(7);
        local.0.store(4 * S, Ordering::Relaxed);
        assert_eq!(
            ph.position().show_ns,
            3 * S,
            "held: clock and timeline disagree"
        );

        // …its transport arrives (playing since cluster time 1000 s)…
        let director_state = TransportState {
            epoch: 1,
            playing: true,
            rate_permille: 1000,
            anchor_cluster_ns: 1000 * S,
            anchor_show_ns: 50 * S,
            loop_ns: None,
        };
        assert!(ph.accept(7, director_state));
        assert_eq!(
            ph.position().show_ns,
            3 * S,
            "held: new timebase not measured yet"
        );

        // …and the first sync sample lands: cluster = local + 1000 s.
        assert!(ph.clock.ingest(7, 4 * S, 1004 * S, 1004 * S, 4 * S));
        assert_eq!(ph.position().show_ns, 54 * S);
        assert!(ph.is_clustered());
    }

    #[test]
    fn director_states_queue_and_stale_ones_are_dropped() {
        let (local, ph) = playhead();
        ph.clock.follow(1);
        ph.clock.ingest(1, 0, 0, 0, 0);
        let stopped = TransportState {
            epoch: 4,
            ..TransportState::STOPPED
        };
        let play_at_2 = TransportState {
            epoch: 5,
            playing: true,
            anchor_cluster_ns: 2 * S,
            ..TransportState::STOPPED
        };
        assert!(ph.accept(1, stopped));
        assert!(ph.accept(1, play_at_2));
        let stale = TransportState {
            epoch: 3,
            ..play_at_2
        };
        assert!(!ph.accept(1, stale), "stale epoch");

        local.0.store(S, Ordering::Relaxed);
        assert!(!ph.position().transport.playing, "not before its anchor");
        local.0.store(3 * S, Ordering::Relaxed);
        assert_eq!(ph.position().show_ns, S);
    }
}
