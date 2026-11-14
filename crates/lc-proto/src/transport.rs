//! Transport state: the deterministic mapping from cluster time to show time.
//!
//! This is the single most important type in the system. Synchronised playback
//! is *not* achieved by sending frames or "now!" messages; it is achieved by
//! every node agreeing on (a) what time it is (`lc-time`) and (b) this small
//! formula. Given both, each Runner independently computes the same frame.

use serde::{Deserialize, Serialize};

use crate::NANOS_PER_SEC;

/// "From cluster time `anchor_cluster_ns` onward, show time is
/// `anchor_show_ns` and advances at `rate_permille`/1000 while `playing`."
///
/// A state only takes effect once cluster time reaches `anchor_cluster_ns`.
/// The Director always anchors new states slightly in the future (the *lead
/// time*), so that every Runner has received the state before it applies and
/// they all switch on the same instant; see `lc_time::TransportTimeline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportState {
    /// Strictly increasing version. A node only accepts a state whose epoch is
    /// higher than the one it holds, which makes delivery order irrelevant and
    /// lets a returning Director and its Runners agree on "whose is newer".
    pub epoch: u64,
    pub playing: bool,
    /// Playback speed in thousandths: 1000 = normal, 500 = half speed,
    /// -1000 = reverse. An integer (not a float) so that every node computes
    /// bit-identical show times.
    pub rate_permille: i32,
    /// Cluster time at which this state takes effect.
    pub anchor_cluster_ns: i64,
    /// Show time at `anchor_cluster_ns`.
    pub anchor_show_ns: i64,
    /// If set, show time wraps into `0..loop_ns`.
    pub loop_ns: Option<i64>,
}

impl TransportState {
    /// The state of a freshly started system: stopped at zero.
    pub const STOPPED: TransportState = TransportState {
        epoch: 0,
        playing: false,
        rate_permille: 1000,
        anchor_cluster_ns: 0,
        anchor_show_ns: 0,
        loop_ns: None,
    };

    /// Show time at `cluster_ns`.
    ///
    /// Before the anchor the show sits at `anchor_show_ns` (callers normally
    /// never ask: the timeline keeps using the previous state until the anchor
    /// is reached). 128-bit intermediate so `elapsed * rate` cannot overflow.
    pub fn show_time_at(&self, cluster_ns: i64) -> i64 {
        // All in i128: these values can come off the wire, and no input may
        // overflow.
        let elapsed = if self.playing {
            (cluster_ns as i128 - self.anchor_cluster_ns as i128).max(0)
        } else {
            0
        };
        let advanced = (elapsed * self.rate_permille as i128).div_euclid(1000);
        let raw = self.anchor_show_ns as i128 + advanced;
        let wrapped = match self.loop_ns {
            Some(len) if len > 0 => raw.rem_euclid(len as i128),
            // No loop: the show cannot run before its own start.
            _ => raw.max(0),
        };
        wrapped.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }
}

impl Default for TransportState {
    fn default() -> Self {
        Self::STOPPED
    }
}

/// A frame rate as an exact fraction, e.g. 60/1 or 60000/1001 (59.94).
///
/// Broadcast rates are not representable in binary floating point, and a show
/// that runs for hours at "59.94" as an `f64` would drift off by whole frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    pub const FPS_60: FrameRate = FrameRate { num: 60, den: 1 };
    pub const FPS_59_94: FrameRate = FrameRate {
        num: 60_000,
        den: 1001,
    };

    /// Index of the frame on screen at `show_ns` (frame 0 covers
    /// `0 .. 1/fps`). Floors, also for negative times.
    pub fn frame_at(&self, show_ns: i64) -> i64 {
        let den = (self.den.max(1) as i128) * NANOS_PER_SEC as i128;
        (show_ns as i128 * self.num as i128).div_euclid(den) as i64
    }

    /// Show time at which `frame` starts (inverse of [`frame_at`](Self::frame_at)),
    /// rounded up so that `frame_at(start_of(f)) == f`.
    pub fn start_of(&self, frame: i64) -> i64 {
        let num = self.num.max(1) as i128;
        let x = frame as i128 * self.den.max(1) as i128 * NANOS_PER_SEC as i128;
        // Ceiling division, correct for negatives too.
        (x.div_euclid(num) + i128::from(x.rem_euclid(num) != 0)) as i64
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::FPS_60
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(anchor_cluster_ns: i64, anchor_show_ns: i64) -> TransportState {
        TransportState {
            epoch: 1,
            playing: true,
            rate_permille: 1000,
            anchor_cluster_ns,
            anchor_show_ns,
            loop_ns: None,
        }
    }

    #[test]
    fn paused_state_does_not_advance() {
        let s = TransportState {
            playing: false,
            anchor_show_ns: 5 * NANOS_PER_SEC,
            ..playing(100, 0)
        };
        assert_eq!(s.show_time_at(100), 5 * NANOS_PER_SEC);
        assert_eq!(s.show_time_at(100 + 60 * NANOS_PER_SEC), 5 * NANOS_PER_SEC);
    }

    #[test]
    fn playing_state_advances_one_to_one() {
        let s = playing(1_000, 2 * NANOS_PER_SEC);
        assert_eq!(s.show_time_at(1_000), 2 * NANOS_PER_SEC);
        assert_eq!(s.show_time_at(1_000 + NANOS_PER_SEC), 3 * NANOS_PER_SEC);
        // Before the anchor it holds the anchor position.
        assert_eq!(s.show_time_at(0), 2 * NANOS_PER_SEC);
    }

    #[test]
    fn rate_scales_and_reverses() {
        let half = TransportState {
            rate_permille: 500,
            ..playing(0, 0)
        };
        assert_eq!(half.show_time_at(2 * NANOS_PER_SEC), NANOS_PER_SEC);

        let reverse = TransportState {
            rate_permille: -1000,
            ..playing(0, 10 * NANOS_PER_SEC)
        };
        assert_eq!(reverse.show_time_at(4 * NANOS_PER_SEC), 6 * NANOS_PER_SEC);
        // Without a loop, reverse playback stops at the start.
        assert_eq!(reverse.show_time_at(60 * NANOS_PER_SEC), 0);
    }

    #[test]
    fn loop_wraps_forward_and_backward() {
        let fwd = TransportState {
            loop_ns: Some(5 * NANOS_PER_SEC),
            ..playing(0, 0)
        };
        assert_eq!(fwd.show_time_at(12 * NANOS_PER_SEC), 2 * NANOS_PER_SEC);

        let back = TransportState {
            rate_permille: -1000,
            loop_ns: Some(5 * NANOS_PER_SEC),
            ..playing(0, 0)
        };
        assert_eq!(back.show_time_at(NANOS_PER_SEC), 4 * NANOS_PER_SEC);
    }

    #[test]
    fn frame_index_is_exact_for_ntsc_rates_over_long_shows() {
        let fps = FrameRate::FPS_59_94;
        // 1001 s of 59.94 is exactly 60 000 frames; a float rate would be off.
        assert_eq!(fps.frame_at(1001 * NANOS_PER_SEC), 60_000);
        assert_eq!(fps.frame_at(1001 * NANOS_PER_SEC - 1), 59_999);
        // Twelve hours in: still exact.
        let twelve_h = 12 * 3600 * NANOS_PER_SEC;
        let f = fps.frame_at(twelve_h);
        assert_eq!(
            f,
            (twelve_h as i128 * 60_000 / (1001 * NANOS_PER_SEC as i128)) as i64
        );
    }

    #[test]
    fn frame_start_round_trips() {
        for fps in [
            FrameRate::FPS_60,
            FrameRate::FPS_59_94,
            FrameRate { num: 25, den: 1 },
        ] {
            for frame in [-3, 0, 1, 59, 60, 1_000_003] {
                let start = fps.start_of(frame);
                assert_eq!(fps.frame_at(start), frame, "{fps:?} frame {frame}");
                assert_eq!(fps.frame_at(start - 1), frame - 1, "{fps:?} frame {frame}");
            }
        }
    }

    #[test]
    fn frame_index_floors_negative_times() {
        assert_eq!(FrameRate::FPS_60.frame_at(-1), -1);
    }
}
