//! Clock handles: the local monotonic clock, the Director's [`MasterClock`]
//! and a Runner's [`ClusterClock`].
//!
//! All clocks are read through the [`LocalClock`] trait so tests can give
//! each simulated node its own offset and drift.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lc_proto::{ClockId, SyncQuality};

use crate::estimator::{ClockEstimator, EstimatorConfig};

/// A monotonic nanosecond counter with an arbitrary zero point.
pub trait LocalClock: Send + Sync + 'static {
    fn now_ns(&self) -> i64;
}

/// The operating system's monotonic clock, counted from process start.
///
/// `std::time::Instant` is `CLOCK_MONOTONIC` on Linux and
/// `QueryPerformanceCounter` on Windows: it never goes backwards and is not
/// affected by the wall clock being set.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        MonotonicClock {
            origin: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalClock for MonotonicClock {
    fn now_ns(&self) -> i64 {
        self.origin.elapsed().as_nanos() as i64
    }
}

/// A clock that is deliberately wrong: shifted by `offset_ns` and running
/// `ppm` parts-per-million fast. Used by tests to stand in for "another
/// machine" whose oscillator disagrees with the Director's.
pub struct SkewedClock {
    inner: Arc<dyn LocalClock>,
    offset_ns: i64,
    ppm: f64,
}

impl SkewedClock {
    pub fn new(inner: Arc<dyn LocalClock>, offset_ns: i64, ppm: f64) -> Self {
        SkewedClock {
            inner,
            offset_ns,
            ppm,
        }
    }
}

impl LocalClock for SkewedClock {
    fn now_ns(&self) -> i64 {
        let t = self.inner.now_ns();
        t + (t as f64 * self.ppm * 1e-6) as i64 + self.offset_ns
    }
}

/// The Director's clock: the definition of cluster time.
///
/// `cluster = local + offset`. The offset is zero for a new timebase and is
/// only ever changed by [`adopt`](Self::adopt), when a restarted Director
/// takes over the timebase its Runners kept alive.
pub struct MasterClock {
    local: Arc<dyn LocalClock>,
    offset_ns: AtomicI64,
    clock_id: AtomicU64,
}

impl MasterClock {
    pub fn new(local: Arc<dyn LocalClock>, clock_id: ClockId) -> Self {
        MasterClock {
            local,
            offset_ns: AtomicI64::new(0),
            clock_id: AtomicU64::new(clock_id),
        }
    }

    pub fn now_ns(&self) -> i64 {
        self.local.now_ns() + self.offset_ns.load(Ordering::Relaxed)
    }

    pub fn clock_id(&self) -> ClockId {
        self.clock_id.load(Ordering::Relaxed)
    }

    /// Continue an existing timebase: become `clock_id`, with cluster time
    /// reading `cluster_now_ns` at this moment.
    pub fn adopt(&self, clock_id: ClockId, cluster_now_ns: i64) {
        self.offset_ns
            .store(cluster_now_ns - self.local.now_ns(), Ordering::Relaxed);
        self.clock_id.store(clock_id, Ordering::Relaxed);
    }
}

/// One consistent reading of a [`ClusterClock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReading {
    pub cluster_ns: i64,
    /// Timebase being followed; `None` for a stand-alone clock.
    pub clock_id: Option<ClockId>,
    /// False between `follow`ing a new timebase and its first sample, when
    /// `cluster_ns` is still the *old* timebase running on.
    pub synced: bool,
}

/// A Runner's view of cluster time: its local clock plus the estimator's
/// current mapping. Cheap to read from the render thread; fed by the sync
/// client task.
///
/// Before any sync it is the identity (`cluster = local`), which is exactly
/// what a stand-alone Runner needs.
pub struct ClusterClock {
    local: Arc<dyn LocalClock>,
    inner: Mutex<Inner>,
}

struct Inner {
    estimator: ClockEstimator,
    /// Timebase being followed. Samples from any other are ignored.
    clock_id: Option<ClockId>,
}

impl ClusterClock {
    pub fn new(local: Arc<dyn LocalClock>) -> Self {
        Self::with_config(local, EstimatorConfig::default())
    }

    pub fn with_config(local: Arc<dyn LocalClock>, cfg: EstimatorConfig) -> Self {
        ClusterClock {
            local,
            inner: Mutex::new(Inner {
                estimator: ClockEstimator::new(cfg),
                clock_id: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // The estimator has no invariants a panic could break half-way, so a
        // poisoned lock is still safe to use — and a render thread must not
        // die because some other thread did.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn local_now_ns(&self) -> i64 {
        self.local.now_ns()
    }

    /// Cluster time now.
    pub fn now_ns(&self) -> i64 {
        self.at_local(self.local.now_ns())
    }

    /// Cluster time at a given local time — e.g. the predicted time of the
    /// next vertical blank, so the frame being rendered shows the content
    /// that is due when it actually reaches the screen.
    pub fn at_local(&self, local_ns: i64) -> i64 {
        self.lock().estimator.cluster_ns(local_ns)
    }

    /// Cluster time at `local_ns` together with which timebase it is in and
    /// whether that timebase has been measured yet — read under one lock, so
    /// the three always belong together. A caller that must not mix
    /// timebases (the playhead) uses this instead of three separate reads.
    pub fn read(&self, local_ns: i64) -> ClockReading {
        let inner = self.lock();
        ClockReading {
            cluster_ns: inner.estimator.cluster_ns(local_ns),
            clock_id: inner.clock_id,
            synced: inner.estimator.is_synced(),
        }
    }

    pub fn clock_id(&self) -> Option<ClockId> {
        self.lock().clock_id
    }

    /// Start following `clock_id`. If it differs from the timebase followed so
    /// far, measurements are discarded and the next sample re-locks (steps).
    /// Returns whether the timebase changed.
    ///
    /// Only the control channel calls this (on `Welcome`): the decision which
    /// Director to follow belongs to the authenticated, reliable channel, not
    /// to whatever answers on the sync port.
    pub fn follow(&self, clock_id: ClockId) -> bool {
        let now = self.local.now_ns();
        let mut inner = self.lock();
        if inner.clock_id == Some(clock_id) {
            return false;
        }
        inner.clock_id = Some(clock_id);
        inner.estimator.reset(now);
        true
    }

    /// Feed one sync exchange. Ignored unless it comes from the followed
    /// timebase. Returns whether the sample was used.
    pub fn ingest(&self, clock_id: ClockId, t1: i64, t2: i64, t3: i64, t4: i64) -> bool {
        let mut inner = self.lock();
        if inner.clock_id != Some(clock_id) {
            return false;
        }
        inner.estimator.add_sample(t1, t2, t3, t4)
    }

    pub fn quality(&self) -> SyncQuality {
        let now = self.local.now_ns();
        self.lock().estimator.quality(now)
    }

    /// Steps taken after initial lock (see [`ClockEstimator::steps`]).
    pub fn steps(&self) -> u64 {
        self.lock().estimator.steps()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock the test moves by hand.
    struct Manual(AtomicI64);
    impl LocalClock for Manual {
        fn now_ns(&self) -> i64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn master_clock_adopts_a_timebase() {
        let local = Arc::new(Manual(AtomicI64::new(1_000)));
        let master = MasterClock::new(local.clone(), 7);
        assert_eq!((master.clock_id(), master.now_ns()), (7, 1_000));

        master.adopt(99, 5_000_000);
        assert_eq!((master.clock_id(), master.now_ns()), (99, 5_000_000));
        local.0.store(1_500, Ordering::Relaxed);
        assert_eq!(
            master.now_ns(),
            5_000_500,
            "keeps running from the adopted time"
        );
    }

    #[test]
    fn cluster_clock_ignores_samples_from_other_timebases() {
        let local = Arc::new(Manual(AtomicI64::new(0)));
        let clock = ClusterClock::new(local.clone());
        assert!(
            !clock.ingest(1, 0, 500, 500, 0),
            "not following anything yet"
        );

        assert!(clock.follow(1));
        assert!(!clock.follow(1), "same timebase is not a change");
        assert!(!clock.ingest(2, 0, 500, 500, 0), "wrong timebase");
        assert!(clock.ingest(1, 0, 500, 500, 0));
        assert_eq!(clock.now_ns(), 500);

        // Switching timebase discards the lock but keeps time running.
        assert!(clock.follow(2));
        assert_eq!(clock.now_ns(), 500);
        assert_eq!(clock.quality().samples, 0);
    }

    #[test]
    fn skewed_clock_runs_fast_and_shifted() {
        let base = Arc::new(Manual(AtomicI64::new(1_000_000_000)));
        let skewed = SkewedClock::new(base, 5, 100.0);
        assert_eq!(skewed.now_ns(), 1_000_000_000 + 100_000 + 5);
    }
}
