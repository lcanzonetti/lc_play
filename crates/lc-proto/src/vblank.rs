//! The shared refresh grid: how frame-locked Runners agree, to the
//! nanosecond, on the instant each picture is for.
//!
//! Frame lock (Quadro Sync) makes every display in the system refresh at the
//! same instants. What it cannot do is make every Runner *choose the same
//! content frame* for a given refresh: each Runner only knows that instant
//! through its own clock and its own measurement of its display, both a
//! little wrong (tens to hundreds of microseconds). Whenever the instant falls
//! close to a content-frame boundary, that small error is enough for two
//! Runners to pick neighbouring frames — the classic "frame-locked but one
//! frame apart" symptom.
//!
//! The grid removes the error instead of shrinking it. The Director publishes
//! one reference refresh instant (`origin_ns`, measured by one Runner) and the
//! refresh period (`period_ps`, in picoseconds so that 59.94 Hz is exact
//! enough over hours). Every Runner then:
//!
//! 1. estimates, with its own clock, when the refresh it is rendering for will
//!    happen — an estimate good to well under half a refresh period;
//! 2. rounds that estimate to the nearest grid line, `k`;
//! 3. uses the grid instant `origin + k × period` — the *same integer* on every
//!    Runner — as the time the picture is for.
//!
//! From there, show time and frame index are pure integer functions of the
//! transport state, so every Runner computes exactly the same frame. Step 2 is
//! only valid if the local refreshes really sit on the grid, i.e. if the
//! displays are frame-locked: [`VblankGrid::snap`] reports how far off they
//! are, and the Runner falls back to its own estimate when that is more than
//! [`ON_GRID_TOLERANCE`] of a period.

use serde::{Deserialize, Serialize};

/// Picoseconds per nanosecond.
const PS_PER_NS: i128 = 1000;

/// A Runner's refreshes count as "on the grid" while they stay within this
/// fraction of a period of a grid line. Frame-locked displays measure within
/// about a millisecond (clock sync plus measurement error); unlocked ones
/// drift through every phase. A quarter period leaves room for the first.
pub const ON_GRID_TOLERANCE: f64 = 0.25;

/// …but they only *join* the grid within this fraction. The gap between the
/// two is hysteresis: an unlocked display drifting past a grid line does not
/// flip in and out (each flip is a step of up to a quarter period in the
/// instant its frames are chosen for).
pub const JOIN_GRID_TOLERANCE: f64 = 0.1;

/// The refresh grid published by the Director.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VblankGrid {
    /// Cluster time of one refresh (a grid line).
    pub origin_ns: i64,
    /// Refresh period, picoseconds (16 683 333 ps... is 59.94 Hz).
    pub period_ps: i64,
    /// Grid lines at or after this cluster time use this grid; earlier ones
    /// use the previous grid. Every Runner switches on the same grid line,
    /// because the decision is made on grid instants, not on local clocks.
    pub valid_from_ns: i64,
    /// Counts grids; for humans and logs.
    pub generation: u64,
}

/// Where a time estimate lands on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapped {
    /// The grid instant (cluster ns) — identical on every Runner for the same
    /// refresh.
    pub instant_ns: i64,
    /// Estimate minus grid instant: how far this Runner's refresh is from the
    /// grid (≈ 0 when frame-locked).
    pub offset_ns: i64,
}

impl VblankGrid {
    /// Whether the numbers can be used at all (they come off the wire).
    pub fn is_valid(&self) -> bool {
        // 1 kHz .. 1 Hz: anything else is not a display refresh.
        (1_000_000_000..=1_000_000_000_000_000).contains(&self.period_ps)
    }

    /// Period in nanoseconds, as a float (for display and tolerances only).
    pub fn period_ns(&self) -> f64 {
        self.period_ps as f64 / PS_PER_NS as f64
    }

    /// The grid instant `k` lines from the origin (floored to the ns: the
    /// same integer everywhere).
    pub fn instant(&self, k: i64) -> i64 {
        let ps = k as i128 * self.period_ps as i128;
        clamp_i64(self.origin_ns as i128 + ps.div_euclid(PS_PER_NS))
    }

    /// The grid line nearest to `estimate_ns`.
    pub fn line_near(&self, estimate_ns: i64) -> i64 {
        let d_ps = (estimate_ns as i128 - self.origin_ns as i128) * PS_PER_NS;
        let p = self.period_ps.max(1) as i128;
        // Round half away from zero would make the two sides asymmetric;
        // floor(x + ½) is the same rule everywhere.
        clamp_i64((2 * d_ps + p).div_euclid(2 * p))
    }

    /// Snap a refresh-time estimate to the grid. `None` if the estimate is
    /// further than [`ON_GRID_TOLERANCE`] periods from any grid line: the
    /// Runner's refreshes are not on this grid (no frame lock, or a different
    /// refresh rate), and the estimate must be used as it is.
    pub fn snap(&self, estimate_ns: i64) -> Option<Snapped> {
        self.snap_within(estimate_ns, ON_GRID_TOLERANCE)
    }

    /// [`snap`](Self::snap) with a given tolerance, in periods.
    pub fn snap_within(&self, estimate_ns: i64, tolerance: f64) -> Option<Snapped> {
        if !self.is_valid() {
            return None;
        }
        let k = self.line_near(estimate_ns);
        let instant_ns = self.instant(k);
        let offset_ns = estimate_ns.saturating_sub(instant_ns);
        (offset_ns.unsigned_abs() as f64 <= tolerance * self.period_ns()).then_some(Snapped {
            instant_ns,
            offset_ns,
        })
    }
}

/// The grids a Runner knows: the one in force and, once the Director has
/// announced a replacement, the next one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GridSchedule {
    pub current: Option<VblankGrid>,
    pub next: Option<VblankGrid>,
}

impl GridSchedule {
    /// Take a grid from the Director.
    pub fn accept(&mut self, grid: VblankGrid) {
        if !grid.is_valid() {
            return;
        }
        let newest = self.next.or(self.current).map_or(0, |g| g.generation);
        if grid.generation <= newest && self.current.is_some() {
            return; // a re-send, or out of order
        }
        match self.current {
            None => self.current = Some(grid),
            Some(_) => self.next = Some(grid),
        }
    }

    /// Snap an estimate, choosing the grid in force for it.
    ///
    /// The switch from `current` to `next` happens at the first grid line of
    /// `current` that is at or after `next.valid_from_ns`. That is decided on
    /// grid instants, so every Runner switches on the same refresh even
    /// though their estimates differ a little.
    ///
    /// `was_on` says whether the caller's refreshes were on the grid last
    /// time: staying on uses [`ON_GRID_TOLERANCE`], joining the stricter
    /// [`JOIN_GRID_TOLERANCE`].
    pub fn snap(&mut self, estimate_ns: i64, was_on: bool) -> Option<Snapped> {
        let current = self.current?;
        if let Some(next) = self.next {
            let line = current.instant(current.line_near(estimate_ns));
            if line >= next.valid_from_ns {
                self.current = Some(next);
                self.next = None;
            }
        }
        let tolerance = if was_on {
            ON_GRID_TOLERANCE
        } else {
            JOIN_GRID_TOLERANCE
        };
        self.current
            .and_then(|g| g.snap_within(estimate_ns, tolerance))
    }
}

/// One output's refresh as measured by its Runner: what the Director builds
/// the grid from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VblankReport {
    /// Cluster time of a recent refresh.
    pub vblank_ns: i64,
    /// Measured refresh period, picoseconds.
    pub period_ps: i64,
    /// Whether the measurement comes from the display driver's own
    /// statistics (exact) rather than from the render loop's timing.
    pub exact: bool,
}

/// Health of one output's presentation, for the operator.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct OutputHealth {
    /// The display's id in the show.
    pub id: String,
    /// The GPU that renders it.
    #[serde(default)]
    pub gpu: String,
    /// How it is presented: `dxgi` (LC Play's own swap chain, with
    /// statistics) or `wgpu`.
    #[serde(default)]
    pub presenter: String,
    /// Measured refresh rate, Hz.
    #[serde(default)]
    pub refresh_hz: Option<f64>,
    /// Frames presented.
    #[serde(default)]
    pub presents: u64,
    /// Frames that reached the screen one or more refreshes after the one
    /// they were rendered for (from the driver's statistics; `dxgi` only).
    #[serde(default)]
    pub missed: u64,
    /// Frames in which a video layer on this output showed an older frame
    /// than the one due, because the decoder was late.
    #[serde(default)]
    pub late_decodes: u64,
    /// How many refreshes after the one it starts rendering in a frame
    /// reaches the screen (measured; `dxgi` only).
    #[serde(default)]
    pub latency_frames: Option<i64>,
    /// NVIDIA present barrier: `off`, `unsupported`, `not_joined`, `client`
    /// (joined, not yet synchronized), `system` (synchronized with the other
    /// outputs of this machine) or `cluster` (synchronized across machines
    /// through frame lock).
    #[serde(default)]
    pub barrier: String,
    /// Distance of this output's refreshes from the Director's grid, ns.
    /// `None` when there is no grid or the output is not on it.
    #[serde(default)]
    pub grid_offset_ns: Option<i64>,
    /// NVIDIA frame-lock frame counter, when available: the same number on
    /// every frame-locked machine at the same refresh.
    #[serde(default)]
    pub frame_counter: Option<u32>,
}

fn clamp_i64(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 59.94 Hz.
    const P: i64 = 16_683_333_333;

    fn grid(origin_ns: i64) -> VblankGrid {
        VblankGrid {
            origin_ns,
            period_ps: P,
            valid_from_ns: 0,
            generation: 1,
        }
    }

    #[test]
    fn estimates_a_little_off_snap_to_the_same_instant() {
        let g = grid(1_000_000_000);
        let truth = g.instant(600); // ten seconds later
        let a = g.snap(truth + 400_000).unwrap();
        let b = g.snap(truth - 700_000).unwrap();
        assert_eq!(a.instant_ns, truth);
        assert_eq!(b.instant_ns, truth);
        assert_eq!((a.offset_ns, b.offset_ns), (400_000, -700_000));
    }

    #[test]
    fn an_unlocked_display_is_not_on_the_grid() {
        let g = grid(0);
        // Half a period off: certainly not frame-locked.
        assert!(g.snap(g.instant(10) + P / 2000).is_none());
    }

    #[test]
    fn grid_instants_do_not_accumulate_rounding() {
        let g = grid(0);
        // After an hour at 59.94 Hz the grid is still exact to the ns:
        // 215 784 periods = 3 599 999 999 999 972 ps... floored.
        let k = 215_784;
        assert_eq!(g.instant(k), (k as i128 * P as i128 / 1000) as i64);
        assert_eq!(g.line_near(g.instant(k)), k);
    }

    #[test]
    fn negative_lines_round_the_same_way() {
        let g = grid(10_000_000_000);
        for k in [-5, -1, 0, 1, 5] {
            assert_eq!(g.line_near(g.instant(k) + 1), k);
            assert_eq!(g.line_near(g.instant(k) - 1), k);
        }
    }

    #[test]
    fn joining_the_grid_is_stricter_than_staying_on_it() {
        let mut s = GridSchedule::default();
        s.accept(grid(0));
        let off = P / 1000 / 5; // a fifth of a period
        assert!(s.snap(grid(0).instant(7) + off, false).is_none());
        assert!(s.snap(grid(0).instant(7) + off, true).is_some());
    }

    #[test]
    fn hostile_grids_are_ignored() {
        let mut s = GridSchedule::default();
        s.accept(VblankGrid {
            period_ps: 0,
            ..grid(0)
        });
        assert!(s.current.is_none());
        let g = VblankGrid {
            origin_ns: i64::MAX,
            ..grid(0)
        };
        // No panic, whatever the numbers.
        let _ = g.snap(i64::MIN);
        let _ = g.instant(i64::MAX);
    }

    #[test]
    fn every_runner_switches_grids_on_the_same_refresh() {
        let old = grid(0);
        // The next grid is the same refresh train re-measured 300 µs later.
        let next = VblankGrid {
            origin_ns: old.instant(1000) + 300_000,
            valid_from_ns: old.instant(1200) - 5_000_000,
            generation: 2,
            ..old
        };
        // Two Runners whose estimates straddle `valid_from` differently.
        let run = |bias: i64| {
            let mut s = GridSchedule::default();
            s.accept(old);
            s.accept(next);
            (1190..1210)
                .map(|k| s.snap(old.instant(k) + bias, true).unwrap().instant_ns)
                .collect::<Vec<_>>()
        };
        assert_eq!(run(4_000_000 / 10), run(-3_000_000 / 10));
    }
}
