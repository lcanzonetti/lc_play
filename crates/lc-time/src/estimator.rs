//! The clock estimator: from noisy four-timestamp exchanges to a smooth,
//! monotonic `local time → cluster time` mapping.
//!
//! ## The measurement
//!
//! One exchange yields four timestamps: the client sends at `t1` (its clock),
//! the server receives at `t2` and replies at `t3` (cluster clock), the client
//! receives at `t4` (its clock). Assuming the two network legs take equally
//! long:
//!
//! ```text
//! offset = ((t2 − t1) + (t3 − t4)) / 2      cluster − local, at local time (t1+t4)/2
//! delay  =  (t4 − t1) − (t3 − t2)           round trip, server processing removed
//! ```
//!
//! The assumption is the weak point: if one leg is slower by `a`, the offset
//! is wrong by `a/2`. Queueing in a switch or a busy scheduler only ever
//! *adds* delay, so the exchanges with the **smallest** round trip are the
//! ones least affected. The estimator therefore keeps a window of samples and
//! fits only the lowest-delay fraction (NTP's clock filter uses the same
//! idea).
//!
//! ## The model
//!
//! Two quartz oscillators differ in frequency by tens of ppm — a millisecond
//! every minute — so a plain offset is not enough. A least-squares line
//! through the selected samples gives `offset` *and* `drift`. Knowing the
//! drift is what makes **holdover** work: with the Director gone, a Runner
//! keeps extrapolating along the line instead of freezing its offset.
//!
//! ## Applying corrections
//!
//! Cluster time drives video. Jumping it forward drops frames and jumping it
//! back repeats them, so a new estimate is not applied directly: the
//! difference is **slewed** in — the mapping runs a few hundred ppm fast or
//! slow until it has caught up, which is invisible. Only an error too large
//! to slew away in reasonable time (first lock, a different Director) is
//! **stepped**.

use std::collections::VecDeque;

use lc_proto::SyncQuality;

/// Tuning. The defaults suit a wired LAN polled at 2 Hz.
#[derive(Debug, Clone)]
pub struct EstimatorConfig {
    /// Samples kept. At 2 Hz, 64 samples span ~32 s: long enough to resolve
    /// drift to about a ppm, short enough to follow a warming-up oscillator.
    pub window: usize,
    /// Fraction of the window (lowest delay first) used for the fit.
    pub keep_fraction: f64,
    /// Drift is only fitted once this many selected samples…
    pub min_drift_samples: usize,
    /// …span at least this much local time; before that a slope would mostly
    /// be noise. Until then only the offset is estimated (as a mean, which
    /// lags a drifting clock by `drift × half the window`), so this should
    /// not be longer than necessary.
    pub min_drift_span_ns: i64,
    /// Sanity clamp on the fitted drift. Real oscillators are within ±100 ppm.
    pub max_drift_ppm: f64,
    /// Errors above this are stepped instead of slewed.
    pub step_threshold_ns: i64,
    /// Fastest the mapping may run off-rate while slewing.
    pub max_slew_ppm: f64,
    /// Time constant of the slew: an error `e` is corrected at `e / slew_time`
    /// (capped by `max_slew_ppm`).
    pub slew_time_ns: i64,
    /// `locked` requires a fitted drift, the fit's RMS residual below this…
    pub lock_error_ns: i64,
    /// …at least this many samples in the window…
    pub lock_min_samples: usize,
    /// …and a sample accepted more recently than this.
    pub lock_max_holdover_ns: i64,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        EstimatorConfig {
            window: 64,
            keep_fraction: 0.5,
            min_drift_samples: 8,
            min_drift_span_ns: 2_000_000_000,
            max_drift_ppm: 500.0,
            step_threshold_ns: 20_000_000,
            max_slew_ppm: 500.0,
            slew_time_ns: 2_000_000_000,
            lock_error_ns: 1_000_000,
            lock_min_samples: 8,
            lock_max_holdover_ns: 5_000_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Local time the offset refers to (midpoint of the exchange).
    local_ns: i64,
    offset_ns: i64,
    delay_ns: i64,
}

#[derive(Debug, Clone)]
pub struct ClockEstimator {
    cfg: EstimatorConfig,
    samples: VecDeque<Sample>,
    /// False until the first sample; until then the mapping is whatever it
    /// was before (identity for a new estimator).
    initialized: bool,

    // The applied mapping, valid from `base_local_ns` on:
    //   offset(t) = base_offset_ns + drift·(t − base) + slew term
    base_local_ns: i64,
    base_offset_ns: i64,
    /// Fractional frequency offset (1e-6 = 1 ppm).
    drift: f64,
    /// Error still to be slewed in (signed, ns)…
    pending_ns: f64,
    /// …at this rate (ns per ns, ≥ 0).
    slew: f64,

    /// Whether `drift` comes from a fit (as opposed to the initial zero).
    drift_fitted: bool,
    last_sample_local_ns: Option<i64>,
    min_delay_ns: i64,
    rms_error_ns: f64,
    steps: u64,
}

impl ClockEstimator {
    pub fn new(cfg: EstimatorConfig) -> Self {
        ClockEstimator {
            cfg,
            samples: VecDeque::new(),
            initialized: false,
            base_local_ns: 0,
            base_offset_ns: 0,
            drift: 0.0,
            pending_ns: 0.0,
            slew: 0.0,
            drift_fitted: false,
            last_sample_local_ns: None,
            min_delay_ns: 0,
            rms_error_ns: 0.0,
            steps: 0,
        }
    }

    /// Forget all measurements (the timebase changed). The mapping itself is
    /// left running, frozen at its current offset, until the first sample of
    /// the new timebase steps it — so time never stalls in between.
    pub fn reset(&mut self, local_now_ns: i64) {
        self.base_offset_ns = self.offset_at(local_now_ns);
        self.base_local_ns = local_now_ns;
        self.drift = 0.0;
        self.pending_ns = 0.0;
        self.slew = 0.0;
        self.samples.clear();
        self.initialized = false;
        self.drift_fitted = false;
        self.last_sample_local_ns = None;
        self.min_delay_ns = 0;
        self.rms_error_ns = 0.0;
    }

    /// Current `cluster − local` offset at local time `local_ns`.
    pub fn offset_at(&self, local_ns: i64) -> i64 {
        let dt = (local_ns - self.base_local_ns) as f64;
        // The slew term only ever moves towards `pending_ns` and stops there,
        // so a Runner in holdover does not keep "correcting" forever.
        let slewed =
            self.pending_ns.signum() * (self.slew * dt.max(0.0)).min(self.pending_ns.abs());
        self.base_offset_ns + (self.drift * dt + slewed).round() as i64
    }

    /// Cluster time at local time `local_ns`.
    pub fn cluster_ns(&self, local_ns: i64) -> i64 {
        local_ns + self.offset_at(local_ns)
    }

    /// Whether at least one sample of the current timebase has been applied.
    pub fn is_synced(&self) -> bool {
        self.initialized
    }

    /// Number of times the mapping was stepped *after* the initial lock. In a
    /// healthy cluster this stays at zero.
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Feed one exchange. Returns `false` if the timestamps are impossible
    /// (reordered or corrupted) and the sample was dropped.
    pub fn add_sample(&mut self, t1: i64, t2: i64, t3: i64, t4: i64) -> bool {
        let (round_trip, server) = (t4 as i128 - t1 as i128, t3 as i128 - t2 as i128);
        let delay = round_trip - server;
        if round_trip < 0 || server < 0 || delay < 0 || delay > i64::MAX as i128 {
            return false;
        }
        let offset = ((t2 as i128 - t1 as i128) + (t3 as i128 - t4 as i128)) / 2;
        let Ok(offset_ns) = i64::try_from(offset) else {
            return false;
        };
        self.samples.push_back(Sample {
            local_ns: t1 + (round_trip / 2) as i64,
            offset_ns,
            delay_ns: delay as i64,
        });
        while self.samples.len() > self.cfg.window.max(1) {
            self.samples.pop_front();
        }
        self.last_sample_local_ns = Some(t4);
        self.refit(t4);
        true
    }

    /// Re-fit the model to the window and steer the applied mapping to it.
    fn refit(&mut self, now_ns: i64) {
        // 1. Select the lowest-delay samples.
        let mut selected: Vec<Sample> = self.samples.iter().copied().collect();
        selected.sort_by_key(|s| s.delay_ns);
        let keep = ((selected.len() as f64 * self.cfg.keep_fraction).ceil() as usize)
            .clamp(1, selected.len());
        selected.truncate(keep);
        self.min_delay_ns = selected[0].delay_ns;

        // 2. Least-squares line through them. Work relative to `now` and to
        //    one sample's offset so the f64 arithmetic handles small numbers
        //    (offsets themselves can be 1e14 ns between two long-running
        //    machines, where f64 would lose sub-µs precision).
        let ref_offset = selected[0].offset_ns;
        let xs: Vec<f64> = selected
            .iter()
            .map(|s| (s.local_ns - now_ns) as f64)
            .collect();
        let ys: Vec<f64> = selected
            .iter()
            .map(|s| (s.offset_ns - ref_offset) as f64)
            .collect();
        let n = keep as f64;
        let (mean_x, mean_y) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
        let span = xs.iter().cloned().fold(f64::MIN, f64::max)
            - xs.iter().cloned().fold(f64::MAX, f64::min);

        let max_drift = self.cfg.max_drift_ppm * 1e-6;
        let drift =
            if keep >= self.cfg.min_drift_samples && span >= self.cfg.min_drift_span_ns as f64 {
                let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
                let sxy: f64 = xs
                    .iter()
                    .zip(&ys)
                    .map(|(x, y)| (x - mean_x) * (y - mean_y))
                    .sum();
                self.drift_fitted = true;
                (sxy / sxx).clamp(-max_drift, max_drift)
            } else {
                // Not enough baseline for a slope yet: keep what we had (zero at
                // first lock) and only estimate the offset.
                self.drift
            };
        // Line value at x = 0, i.e. the offset right now.
        let intercept = mean_y - drift * mean_x;
        self.rms_error_ns = (xs
            .iter()
            .zip(&ys)
            .map(|(x, y)| (y - intercept - drift * x).powi(2))
            .sum::<f64>()
            / n)
            .sqrt();
        let target_ns = ref_offset + intercept.round() as i64;

        // 3. Steer the applied mapping towards the target.
        let applied_ns = self.offset_at(now_ns);
        let error = target_ns - applied_ns;
        if !self.initialized || error.abs() > self.cfg.step_threshold_ns {
            if self.initialized {
                self.steps += 1;
            }
            self.base_offset_ns = target_ns;
            self.pending_ns = 0.0;
            self.slew = 0.0;
            self.initialized = true;
        } else {
            self.base_offset_ns = applied_ns;
            self.pending_ns = error as f64;
            self.slew = (error.abs() as f64 / self.cfg.slew_time_ns.max(1) as f64)
                .min(self.cfg.max_slew_ppm * 1e-6);
        }
        self.base_local_ns = now_ns;
        self.drift = drift;
    }

    /// A status snapshot for health reporting.
    pub fn quality(&self, local_now_ns: i64) -> SyncQuality {
        let holdover_ns = self
            .last_sample_local_ns
            .map(|t| (local_now_ns - t).max(0))
            .unwrap_or(0);
        let locked = self.initialized
            && self.drift_fitted
            && self.samples.len() >= self.cfg.lock_min_samples
            && self.rms_error_ns < self.cfg.lock_error_ns as f64
            && holdover_ns < self.cfg.lock_max_holdover_ns;
        SyncQuality {
            locked,
            samples: self.samples.len() as u32,
            offset_ns: self.offset_at(local_now_ns),
            drift_ppm: self.drift * 1e6,
            rtt_ns: self.min_delay_ns,
            error_ns: self.rms_error_ns.round() as i64,
            holdover_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The estimator is tested against a simulated link where the truth is
    //! known: a Runner clock with a chosen offset and drift, and network legs
    //! with chosen (independent, random) delays.

    use super::*;

    const MS: i64 = 1_000_000;
    const US: i64 = 1_000;
    const S: i64 = 1_000_000_000;

    /// Small deterministic PRNG (xorshift) so tests are reproducible without
    /// a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        /// Uniform in `0..max`.
        fn below(&mut self, max: i64) -> i64 {
            (self.next() % max.max(1) as u64) as i64
        }
    }

    /// A simulated Runner. "True time" *is* cluster time; the Runner's local
    /// clock runs at `1 + ppm` and is shifted by `offset_ns`.
    struct Sim {
        offset_ns: i64,
        ppm: f64,
        base_delay_ns: i64,
        jitter_ns: i64,
        /// One exchange in `spike_every` gets `spike_ns` added to one leg.
        spike_every: u64,
        spike_ns: i64,
        rng: Rng,
        est: ClockEstimator,
    }

    impl Sim {
        fn new(offset_ns: i64, ppm: f64) -> Self {
            Sim {
                offset_ns,
                ppm,
                base_delay_ns: 150 * US,
                jitter_ns: 0,
                spike_every: 0,
                spike_ns: 0,
                rng: Rng(0x9E37_79B9_7F4A_7C15),
                est: ClockEstimator::new(EstimatorConfig::default()),
            }
        }

        fn local(&self, true_ns: i64) -> i64 {
            true_ns + (true_ns as f64 * self.ppm * 1e-6) as i64 - self.offset_ns
        }

        fn leg(&mut self) -> i64 {
            self.base_delay_ns + self.rng.below(self.jitter_ns)
        }

        /// One request/response starting at true time `t`.
        fn exchange(&mut self, t: i64) {
            let (mut up, down) = (self.leg(), self.leg());
            if self.spike_every > 0 && self.rng.next().is_multiple_of(self.spike_every) {
                up += self.spike_ns;
            }
            let t2 = t + up;
            let t3 = t2 + 20 * US;
            self.est
                .add_sample(self.local(t), t2, t3, self.local(t3 + down));
        }

        /// Poll every `period` from `from` to `to`.
        fn run(&mut self, from: i64, to: i64, period: i64) {
            let mut t = from;
            while t < to {
                self.exchange(t);
                t += period;
            }
        }

        /// Estimated minus true cluster time, at true time `t`.
        fn error_at(&self, t: i64) -> i64 {
            self.est.cluster_ns(self.local(t)) - t
        }
    }

    #[test]
    fn unsynced_estimator_is_the_identity() {
        let est = ClockEstimator::new(EstimatorConfig::default());
        assert_eq!(est.cluster_ns(12_345), 12_345);
        assert!(!est.quality(12_345).locked);
    }

    #[test]
    fn first_sample_locks_a_static_offset_exactly_on_a_symmetric_link() {
        let mut sim = Sim::new(3600 * S, 0.0);
        sim.exchange(10 * S);
        assert_eq!(sim.error_at(10 * S + MS), 0);
    }

    #[test]
    fn tracks_drift_through_jitter() {
        // 80 ppm fast, legs 150–450 µs each, independently random.
        let mut sim = Sim::new(-7 * S, 80.0);
        sim.jitter_ns = 300 * US;
        sim.run(0, 60 * S, S / 2);

        let q = sim.est.quality(sim.local(60 * S));
        assert!(q.locked, "{q:?}");
        assert!(
            (q.drift_ppm + 80.0).abs() < 5.0,
            "drift {} ppm",
            q.drift_ppm
        );
        // Worst case over the following seconds, sampled between polls.
        let worst = (0..200)
            .map(|i| sim.error_at(60 * S + i * 10 * MS).abs())
            .max()
            .unwrap();
        assert!(worst < 100 * US, "error {worst} ns");
        assert_eq!(sim.est.steps(), 0, "never stepped after initial lock");
    }

    #[test]
    fn delay_spikes_are_filtered_out() {
        // One exchange in four has 20 ms extra on the request leg. Taken at
        // face value each would shift the offset by 10 ms.
        let mut sim = Sim::new(5 * S, -30.0);
        sim.jitter_ns = 100 * US;
        sim.spike_every = 4;
        sim.spike_ns = 20 * MS;
        sim.run(0, 60 * S, S / 2);
        let worst = (0..200)
            .map(|i| sim.error_at(60 * S + i * 10 * MS).abs())
            .max()
            .unwrap();
        assert!(worst < 100 * US, "error {worst} ns");
        assert_eq!(sim.est.steps(), 0);
    }

    #[test]
    fn holdover_extrapolates_drift() {
        // Lock for a minute on a 100 ppm oscillator, then lose the Director
        // for five minutes. A frozen offset would be 30 ms (two frames) off;
        // extrapolating the drift must stay well inside one frame.
        let mut sim = Sim::new(0, 100.0);
        sim.jitter_ns = 200 * US;
        sim.run(0, 60 * S, S / 2);
        let err = sim.error_at(360 * S).abs();
        assert!(err < 2 * MS, "holdover error after 5 min: {err} ns");
        let q = sim.est.quality(sim.local(360 * S));
        assert!(!q.locked, "must report loss of lock");
        assert!(q.holdover_ns > 290 * S);
    }

    #[test]
    fn cluster_time_is_monotonic_while_slewing() {
        let mut sim = Sim::new(42 * S, 60.0);
        sim.jitter_ns = 400 * US;
        let mut last = i64::MIN;
        // One step per simulated millisecond; a poll every 500 steps.
        for step in 0..30_000u64 {
            let t = step as i64 * MS;
            let since_poll = step % 500;
            if since_poll == 0 {
                sim.exchange(t);
            }
            // The simulated exchange completes ~1 ms after `t`; only read the
            // clock once it is in the past, as a real caller would.
            if since_poll >= 2 {
                let now = sim.est.cluster_ns(sim.local(t));
                assert!(now >= last, "went backwards at t={t}: {last} -> {now}");
                last = now;
            }
        }
        assert_eq!(sim.est.steps(), 0);
    }

    #[test]
    fn small_errors_are_slewed_and_gross_errors_stepped() {
        let mut sim = Sim::new(0, 0.0);
        sim.run(0, 10 * S, S / 2);
        assert_eq!(sim.error_at(10 * S), 0);

        // The Runner's clock is suddenly 5 ms off (below the step threshold).
        sim.offset_ns += 5 * MS;
        sim.est.samples.clear(); // old samples describe the old offset
        sim.exchange(10 * S);
        let just_after = sim.error_at(10 * S + MS).abs();
        assert!(just_after > 4 * MS, "must not jump: {just_after}");
        sim.run(10 * S + S / 2, 40 * S, S / 2);
        assert!(
            sim.error_at(40 * S).abs() < 50 * US,
            "slewed in: {}",
            sim.error_at(40 * S)
        );
        assert_eq!(sim.est.steps(), 0);

        // A one-second error is far beyond slewing: stepped at once.
        sim.offset_ns += S;
        sim.est.samples.clear();
        sim.exchange(41 * S);
        assert!(sim.error_at(41 * S + MS).abs() < 50 * US);
        assert_eq!(sim.est.steps(), 1);
    }

    #[test]
    fn reset_keeps_time_running_until_the_new_timebase_locks() {
        let mut sim = Sim::new(9 * S, 0.0);
        sim.run(0, 5 * S, S / 2);
        let before = sim.est.cluster_ns(sim.local(5 * S));
        sim.est.reset(sim.local(5 * S));
        assert_eq!(sim.est.cluster_ns(sim.local(5 * S)), before);
        assert_eq!(sim.est.cluster_ns(sim.local(6 * S)), before + S);
        assert_eq!(sim.est.quality(sim.local(6 * S)).samples, 0);
        // First sample of the new timebase is a (re)lock, not a counted step.
        sim.offset_ns = -100 * S;
        sim.exchange(6 * S);
        assert_eq!(sim.error_at(6 * S + MS), 0);
        assert_eq!(sim.est.steps(), 0);
    }

    #[test]
    fn impossible_timestamps_are_rejected() {
        let mut est = ClockEstimator::new(EstimatorConfig::default());
        assert!(!est.add_sample(100, 0, 0, 50), "t4 before t1");
        assert!(!est.add_sample(0, 100, 50, 10), "t3 before t2");
        assert!(
            !est.add_sample(0, 0, 100, 50),
            "server time exceeds round trip"
        );
        assert_eq!(est.quality(0).samples, 0);
    }
}
