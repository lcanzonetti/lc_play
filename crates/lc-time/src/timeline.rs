//! The transport timeline: which [`TransportState`] is in force *when*.
//!
//! The Director anchors every change a little in the future. A Runner
//! therefore usually holds two states for a moment: the one in force and the
//! one about to take over. `TransportTimeline` keeps both and picks by
//! cluster time, so the switch happens on the anchor instant on every node,
//! not whenever the message happened to arrive.

use lc_proto::TransportState;

#[derive(Debug, Clone)]
pub struct TransportTimeline {
    /// Sorted by ascending epoch; never empty. `states[0]` is the fallback
    /// used when no state's anchor has been reached yet.
    states: Vec<TransportState>,
}

impl TransportTimeline {
    pub fn new(initial: TransportState) -> Self {
        TransportTimeline {
            states: vec![initial],
        }
    }

    /// Offer a state received from the Director. Returns `false` (and ignores
    /// it) unless its epoch is newer than everything already known — which
    /// makes duplicated and reordered deliveries harmless.
    pub fn push(&mut self, state: TransportState) -> bool {
        if state.epoch <= self.latest().epoch {
            return false;
        }
        self.states.push(state);
        true
    }

    /// Forget everything and start from `state`. Used when the timebase
    /// itself changes (a different Director), where epochs and anchors of the
    /// old states are meaningless.
    pub fn replace(&mut self, state: TransportState) {
        self.states.clear();
        self.states.push(state);
    }

    /// The newest state known, whether or not it has taken effect yet.
    pub fn latest(&self) -> TransportState {
        *self.states.last().expect("timeline is never empty")
    }

    /// The state in force at `cluster_ns`: the newest one whose anchor has
    /// been reached.
    pub fn state_at(&self, cluster_ns: i64) -> TransportState {
        self.states
            .iter()
            .rev()
            .find(|s| s.anchor_cluster_ns <= cluster_ns)
            .copied()
            .unwrap_or(self.states[0])
    }

    /// Show time at `cluster_ns`.
    pub fn show_time_at(&self, cluster_ns: i64) -> i64 {
        self.state_at(cluster_ns).show_time_at(cluster_ns)
    }

    /// Drop states that can no longer be selected at or after `cluster_ns`.
    /// Call occasionally; the list stays at one or two entries.
    pub fn prune(&mut self, cluster_ns: i64) {
        let current = self.state_at(cluster_ns).epoch;
        self.states.retain(|s| s.epoch >= current);
    }
}

/// An operator-level change to the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportChange {
    Play,
    Pause,
    /// Jump to an absolute show time.
    Seek(i64),
    /// Jump relative to the current position, clamped at zero.
    SeekBy(i64),
    SetRate(i32),
    SetLoop(Option<i64>),
    /// Stop: pause at zero. With `deactivate` the timeline also goes off
    /// stage (an auxiliary timeline); without, it stays visible at its
    /// first frame (the main timeline).
    Stop {
        deactivate: bool,
    },
}

/// Derive the state that follows `base` when `change` takes effect at
/// `at_cluster_ns`.
///
/// The new state is anchored so that show time is *continuous* across the
/// switch for everything except an explicit seek: its `anchor_show_ns` is
/// exactly where `base` will be at the anchor instant.
pub fn next_state(
    base: &TransportState,
    change: TransportChange,
    at_cluster_ns: i64,
) -> TransportState {
    let position = base.show_time_at(at_cluster_ns);
    let mut next = TransportState {
        epoch: base.epoch + 1,
        anchor_cluster_ns: at_cluster_ns,
        anchor_show_ns: position,
        ..*base
    };
    match change {
        TransportChange::Play => {
            next.playing = true;
            next.active = true; // playing a stopped task brings it on stage
        }
        TransportChange::Stop { deactivate } => {
            next.playing = false;
            next.anchor_show_ns = 0;
            next.active = !deactivate;
        }
        TransportChange::Pause => next.playing = false,
        TransportChange::Seek(show_ns) => next.anchor_show_ns = show_ns.max(0),
        TransportChange::SeekBy(delta) => {
            next.anchor_show_ns = position.saturating_add(delta).max(0)
        }
        TransportChange::SetRate(rate_permille) => next.rate_permille = rate_permille,
        TransportChange::SetLoop(loop_ns) => next.loop_ns = loop_ns.filter(|&l| l > 0),
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use lc_proto::NANOS_PER_SEC as S;

    #[test]
    fn scheduled_state_takes_over_exactly_at_its_anchor() {
        let mut tl = TransportTimeline::new(TransportState::STOPPED);
        let play = next_state(&tl.latest(), TransportChange::Play, 10 * S);
        assert!(tl.push(play));

        // Still stopped right up to the anchor…
        assert!(!tl.state_at(10 * S - 1).playing);
        assert_eq!(tl.show_time_at(10 * S - 1), 0);
        // …playing from the anchor on.
        assert!(tl.state_at(10 * S).playing);
        assert_eq!(tl.show_time_at(11 * S), S);
    }

    #[test]
    fn stale_and_duplicate_states_are_ignored() {
        let mut tl = TransportTimeline::new(TransportState::STOPPED);
        let play = next_state(&tl.latest(), TransportChange::Play, S);
        assert!(tl.push(play));
        assert!(!tl.push(play), "duplicate");
        assert!(!tl.push(TransportState::STOPPED), "older epoch");
        assert_eq!(tl.latest(), play);
    }

    #[test]
    fn pause_freezes_at_the_position_reached() {
        let mut tl = TransportTimeline::new(TransportState::STOPPED);
        tl.push(next_state(&tl.latest(), TransportChange::Play, S));
        // Pause decided at 3.9 s, effective at 4 s: the show has played 3 s.
        let base = tl.state_at(4 * S);
        tl.push(next_state(&base, TransportChange::Pause, 4 * S));
        assert_eq!(tl.show_time_at(4 * S), 3 * S);
        assert_eq!(tl.show_time_at(100 * S), 3 * S);
    }

    #[test]
    fn seek_keeps_play_state_and_rate_change_is_continuous() {
        let playing = next_state(&TransportState::STOPPED, TransportChange::Play, 0);
        let sought = next_state(&playing, TransportChange::Seek(60 * S), 5 * S);
        assert!(sought.playing);
        assert_eq!(sought.show_time_at(6 * S), 61 * S);

        let slow = next_state(&sought, TransportChange::SetRate(500), 7 * S);
        assert_eq!(slow.show_time_at(7 * S), 62 * S, "no jump at the switch");
        assert_eq!(slow.show_time_at(9 * S), 63 * S);

        let back = next_state(&slow, TransportChange::SeekBy(-1000 * S), 9 * S);
        assert_eq!(back.anchor_show_ns, 0, "relative seek clamps at zero");
    }

    #[test]
    fn stop_rewinds_and_optionally_leaves_the_stage() {
        let playing = next_state(&TransportState::INACTIVE, TransportChange::Play, 0);
        assert!(playing.active && playing.playing, "play activates a task");

        let stopped = next_state(&playing, TransportChange::Stop { deactivate: true }, 5 * S);
        assert!(!stopped.active && !stopped.playing);
        assert_eq!(stopped.show_time_at(9 * S), 0);

        let main = next_state(&playing, TransportChange::Stop { deactivate: false }, 5 * S);
        assert!(main.active, "the main timeline stays on stage");
        assert_eq!(main.show_time_at(9 * S), 0);
    }

    #[test]
    fn prune_keeps_the_state_in_force() {
        let mut tl = TransportTimeline::new(TransportState::STOPPED);
        tl.push(next_state(&tl.latest(), TransportChange::Play, S));
        tl.push(next_state(&tl.latest(), TransportChange::Pause, 2 * S));
        tl.prune(S + 1);
        assert!(tl.state_at(S + 1).playing);
        assert!(!tl.state_at(2 * S).playing);
        tl.prune(3 * S);
        assert_eq!(tl.states.len(), 1);
    }
}
