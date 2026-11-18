//! When will this frame be on screen? Per-output refresh timing.
//!
//! The content a Runner draws must be the content due at the instant the
//! picture *appears*, not the instant rendering starts: between the two lie
//! one or more refreshes (the swap chain's queue), and how many depends on the
//! machine and its load. Sampling the show clock at "now" therefore puts two
//! Runners one frame apart even when their clocks agree perfectly.
//!
//! [`RefreshClock`] models one display's refreshes as a straight line,
//! `t(n) = phase + n × period` in local nanoseconds, fitted by least squares
//! to recent observations. Observations come either
//!
//! * from the display driver (`exact`): the time of a refresh (DXGI frame
//!   statistics) — what LC Play's own swap chain uses on Windows. The
//!   driver's timestamps are exact, but its refresh *counter* is not: on a
//!   Windows laptop it lost one refresh at a time whenever frames were
//!   repeated (a 60 Hz output whose window lacked the focus), each loss
//!   shifting the line by a whole period and the fitted period from 16.67 to
//!   18.8 ms. So refreshes are numbered from the timestamps, and the driver's
//!   count only serves before there is a period to number with;
//! * or from the render loop (`tick`): the moment the swap chain let the loop
//!   continue, which follows the refreshes with some scheduling jitter. The
//!   refresh number is inferred from the elapsed time.
//!
//! [`PresentTracker`] closes the loop where the driver reports which refresh
//! each present actually reached: it learns how many refreshes after the
//! predicted one frames really land (the queue depth as it is, not as
//! configured), and counts the frames that landed later than that.

use std::collections::VecDeque;

/// Observations kept for the fit: about two seconds at 60 Hz.
const WINDOW: usize = 120;
/// Fewer observations than this and there is no estimate yet.
const MIN_SAMPLES: usize = 8;
/// Between two driver reports, the longest gap still numbered from time
/// (see [`RefreshClock::observe_refresh`]).
const LONG_GAP_NS: i64 = 1_000_000_000;
/// Plausible refresh periods: 20 Hz .. 500 Hz.
const MIN_PERIOD_NS: f64 = 2_000_000.0;
const MAX_PERIOD_NS: f64 = 50_000_000.0;

/// A display's refresh train, fitted from observations.
#[derive(Debug, Clone, Default)]
pub struct RefreshClock {
    /// (refresh number, local ns).
    samples: VecDeque<(i64, i64)>,
    /// Current fit: local time of refresh 0, and the period. Kept as f64:
    /// the values are a fit, not a schedule, and are converted back to
    /// integer nanoseconds at the edge.
    fit: Option<(f64, f64)>,
    /// Whether the samples come from the driver.
    exact: bool,
    /// The driver's counter at the last driver sample.
    driver_last: Option<i64>,
    /// This clock's refresh number minus the driver's, at the last driver
    /// sample: translates refresh numbers the driver reports (the refresh a
    /// present reached) into this clock's numbering.
    driver_offset: i64,
    /// Before there is a fit: the shortest time per driver count between
    /// consecutive reports since the last restart, a provisional period.
    shortest: Option<f64>,
}

impl RefreshClock {
    /// A refresh reported by the driver: its counter value and time.
    ///
    /// The refresh is numbered from its time: the last sample's number plus
    /// the elapsed time in periods, rounded. The driver's counter only tells
    /// whether this is a new report, and, before there is a fit, gives a
    /// provisional period (see `shortest`). Rounding needs the period right
    /// to within half a refresh over the gap between reports: at 60 Hz a
    /// one-second gap tolerates an error of 0.8 % (0.3 % at 165 Hz), far more
    /// than a fit of exact timestamps has. Over a longer gap (a hidden
    /// window, a debugger) an error too small to see could add up to half a
    /// refresh, so after [`LONG_GAP_NS`] the fit starts again and the report
    /// is numbered by the driver's count: across a stall the counts it may
    /// have lost matter less than a refresh numbered wrong next to the
    /// window's correctly numbered samples.
    pub fn observe_refresh(&mut self, count: i64, local_ns: i64) {
        if !self.exact {
            // Switching from inferred to exact numbering: start over.
            self.restart();
            self.exact = true;
            self.driver_last = None;
        }
        let n = match (self.samples.back().copied(), self.driver_last) {
            (Some((last_n, last_t)), Some(last_count)) => {
                if count == last_count {
                    return; // the same report, read again
                }
                let (dt, dc) = (local_ns - last_t, count - last_count);
                if dt <= 0 {
                    // The clock went backwards: time cannot number this one.
                    self.restart();
                    last_n + dc.max(1)
                } else if dt > LONG_GAP_NS || dc < 0 {
                    // A long gap (see above), or a new counter (swap chain
                    // re-created, mode change) whose old line may not hold:
                    // fit a new line, numbered on from the old so that
                    // refreshes predicted before keep their meaning. A new
                    // counter's count says nothing about the gap, so then
                    // time numbers it, with the old period.
                    let by_time = self
                        .period()
                        .or(self.shortest)
                        .map(|p| ((dt as f64 / p).round() as i64).max(1));
                    self.restart();
                    last_n + if dc > 0 { dc } else { by_time.unwrap_or(1) }
                } else {
                    if dc > 0 {
                        // Lost counts only ever lengthen time-per-count, so
                        // the shortest seen is the best provisional period.
                        let ratio = dt as f64 / dc as f64;
                        if (MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&ratio) {
                            self.shortest = Some(self.shortest.map_or(ratio, |s| s.min(ratio)));
                        }
                    }
                    let step = match self.period().or(self.shortest) {
                        Some(p) => ((dt as f64 / p).round() as i64).max(1),
                        None => dc.max(1),
                    };
                    last_n + step
                }
            }
            _ => count,
        };
        self.driver_last = Some(count);
        self.samples.push_back((n, local_ns));
        while self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
        if self.fit.is_none() {
            // No line yet: the samples so far were numbered with what was
            // known then. Number them again from their times with the best
            // provisional period, so that a count lost early on cannot bend
            // the first fit.
            if let Some(p) = self.shortest {
                let (n0, t0) = self.samples[0];
                for s in self.samples.iter_mut() {
                    s.0 = n0 + ((s.1 - t0) as f64 / p).round() as i64;
                }
            }
        }
        self.refit();
        let last_n = self.samples.back().map_or(n, |s| s.0);
        self.driver_offset = last_n - count;
    }

    /// Forget the samples and the provisional period (not the numbering:
    /// the next sample is numbered on from the last).
    fn restart(&mut self) {
        self.samples.clear();
        self.shortest = None;
    }

    /// This clock's number for a refresh the driver numbered `count`, such as
    /// the refresh a present reached. Translated with the offset of the
    /// latest report, so a count the driver lost between that refresh and the
    /// report would put it one refresh early. In practice they are the same
    /// refresh (all 3480 reports of the laptop capture in `testdata`).
    pub fn from_driver(&self, count: i64) -> i64 {
        count + self.driver_offset
    }

    /// The render loop was released by the swap chain at `local_ns`.
    pub fn observe_tick(&mut self, local_ns: i64) {
        if self.exact {
            return; // the driver's numbers are better
        }
        let n = match (self.samples.back(), self.period()) {
            (None, _) => 0,
            (Some(&(last_n, last_t)), period) => {
                let dt = (local_ns - last_t) as f64;
                if dt <= 0.0 || dt > 1e9 {
                    // Stalled (window hidden, debugger, ...): start over.
                    self.samples.clear();
                    0
                } else {
                    // Before there is a fit, assume consecutive refreshes.
                    let step = period.map_or(1.0, |p| (dt / p).round().max(1.0));
                    last_n + step as i64
                }
            }
        };
        self.push(n, local_ns);
    }

    fn push(&mut self, n: i64, t: i64) {
        self.samples.push_back((n, t));
        while self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
        self.refit();
    }

    fn refit(&mut self) {
        self.fit = None;
        if self.samples.len() < MIN_SAMPLES {
            return;
        }
        // Least squares of t on n, relative to the first sample to keep the
        // numbers small.
        let (n0, t0) = self.samples[0];
        let m = self.samples.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for &(n, t) in &self.samples {
            let (x, y) = ((n - n0) as f64, (t - t0) as f64);
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denom = m * sxx - sx * sx;
        if denom <= 0.0 {
            return;
        }
        let period = (m * sxy - sx * sy) / denom;
        if !(MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&period) {
            return;
        }
        let intercept = (sy - period * sx) / m;
        // Local time of refresh 0.
        self.fit = Some((t0 as f64 + intercept - period * n0 as f64, period));
    }

    /// Measured refresh period, ns.
    pub fn period(&self) -> Option<f64> {
        self.fit.map(|(_, p)| p)
    }

    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Local time of refresh `n`.
    pub fn time_of(&self, n: i64) -> Option<i64> {
        self.fit
            .map(|(phase, period)| (phase + period * n as f64).round() as i64)
    }

    /// The first refresh strictly after `local_ns`: its number and time.
    pub fn next_after(&self, local_ns: i64) -> Option<(i64, i64)> {
        let (phase, period) = self.fit?;
        let n = ((local_ns as f64 - phase) / period).floor() as i64 + 1;
        Some((n, self.time_of(n)?))
    }

    /// The most recent refresh: (number, local time).
    pub fn last_refresh(&self, local_ns: i64) -> Option<(i64, i64)> {
        let (n, _) = self.next_after(local_ns)?;
        Some((n - 1, self.time_of(n - 1)?))
    }
}

/// Which refresh presents really reach, learnt from the driver's reports.
///
/// The lag is measured against the *uncorrected* prediction (`base`), never
/// against the corrected target: otherwise honouring the correction would
/// make the measured lag 0, the correction would fall back to 0, frames would
/// land late again, and the correction would flip every frame.
#[derive(Debug, Clone, Default)]
pub struct PresentTracker {
    /// Presents not yet reported: (present id, uncorrected prediction, the
    /// correction that was applied to it).
    pending: VecDeque<(u32, i64, i64)>,
    /// Recent "landed minus uncorrected prediction" values, in refreshes.
    lags: VecDeque<i64>,
    /// Frames that landed later than the learnt latency.
    pub missed: u64,
    pub presents: u64,
}

impl PresentTracker {
    /// How many extra refreshes frames take beyond the prediction, once
    /// enough have been reported. Every Runner in a present barrier sees the
    /// same landing refreshes, so they learn the same correction.
    pub fn correction(&self) -> i64 {
        if self.lags.len() < MIN_SAMPLES {
            return 0;
        }
        let mut v: Vec<i64> = self.lags.iter().copied().collect();
        v.sort_unstable();
        v[v.len() / 2].clamp(0, 4)
    }

    /// A frame was presented as `present_id`. `base` is the refresh it was
    /// predicted to reach without correction; it was rendered for
    /// `base + correction`.
    pub fn presented(&mut self, present_id: u32, base: i64, correction: i64) {
        self.presents += 1;
        self.pending.push_back((present_id, base, correction));
        while self.pending.len() > 16 {
            self.pending.pop_front();
        }
    }

    /// The driver says present `present_id` reached the screen at refresh
    /// `refresh`.
    pub fn landed(&mut self, present_id: u32, refresh: i64) {
        while let Some(&(id, base, correction)) = self.pending.front() {
            // Wrapping comparison: present ids are 32-bit counters.
            let behind = present_id.wrapping_sub(id) as i32;
            if behind < 0 {
                break; // not reported yet
            }
            self.pending.pop_front();
            if behind == 0 {
                let lag = refresh - base;
                // Later than the refresh it was rendered for: a miss.
                if lag > correction && self.lags.len() >= MIN_SAMPLES {
                    self.missed += 1;
                }
                self.lags.push_back(lag);
                while self.lags.len() > 64 {
                    self.lags.pop_front();
                }
                break;
            }
        }
    }
}

/// One frame's raw timing, as the driver reported it (see [`RawLog`]).
// Only the Windows swap chain reports refreshes.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub struct RawRow {
    /// Output size, to tell outputs apart in one file.
    pub width: u32,
    pub height: u32,
    /// Local time when the swap chain released the loop, and how long the
    /// loop waited for it.
    pub wake_ns: i64,
    pub wait_ns: i64,
    /// `DXGI_FRAME_STATISTICS`, the QPC time converted to local time.
    pub sync_refresh: i64,
    pub sync_ns: i64,
    pub present_count: u32,
    pub present_refresh: i64,
    /// The clock's own number for `sync_refresh` (see
    /// [`RefreshClock::observe_refresh`]): where it runs ahead of the
    /// driver's, the driver lost counts.
    pub clock_refresh: i64,
    /// The clock's period estimate and the tracker's correction after this
    /// report was taken in.
    pub period_ns: Option<f64>,
    pub correction: i64,
}

/// Opt-in CSV of every refresh report the driver gives
/// (`LCPLAY_VSYNC_LOG=<file.csv>`), for diagnosing timing on a machine.
///
/// Why raw rows rather than a smarter estimate: when the period estimate
/// drifts (16.7 ms becoming 17–19 ms on a 60 Hz screen), a few seconds of
/// samples cannot tell "the reports arrive late" from "the refresh counter
/// does not count this monitor's refreshes" — a steady lateness ramp and a
/// longer period are the same straight line. Over a whole test, laid next to
/// PresentMon's capture of the same run, the raw rows can: the counter's
/// steps against QPC time show whether it advances once per 16.7 ms.
// Only the Windows swap chain reports refreshes.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RawLog {
    /// The file, and the rows written to it.
    out: std::sync::Mutex<(std::io::BufWriter<std::fs::File>, u64)>,
}

// Only the Windows swap chain reports refreshes.
#[cfg_attr(not(windows), allow(dead_code))]
impl RawLog {
    const HEADER: &'static str = "width,height,wake_ns,wait_ms,sync_refresh,sync_ns,\
        present_count,present_refresh,period_ms,correction,clock_refresh";

    /// Create (truncate) `path` and write the header.
    pub fn create(path: &std::path::Path) -> std::io::Result<Self> {
        use std::io::Write as _;
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(out, "{}", Self::HEADER)?;
        Ok(RawLog {
            out: std::sync::Mutex::new((out, 0)),
        })
    }

    /// The log named by `LCPLAY_VSYNC_LOG`, opened on first use; `None` when
    /// the variable is unset or the file cannot be created (said once).
    pub fn global() -> Option<&'static RawLog> {
        static LOG: std::sync::OnceLock<Option<RawLog>> = std::sync::OnceLock::new();
        LOG.get_or_init(|| {
            let path = std::env::var_os("LCPLAY_VSYNC_LOG").filter(|v| !v.is_empty())?;
            let path = std::path::PathBuf::from(path);
            match RawLog::create(&path) {
                Ok(log) => {
                    let shown = std::fs::canonicalize(&path).unwrap_or(path);
                    tracing::info!("vsync log: writing {}", shown.display());
                    Some(log)
                }
                Err(e) => {
                    tracing::warn!("vsync log: cannot create {}: {e}", path.display());
                    None
                }
            }
        })
        .as_ref()
    }

    /// Append a row. Flushed every 60 rows (about a second), so a run that
    /// is killed loses at most that much. Write errors are ignored: this is
    /// a diagnostic and must never disturb the output.
    pub fn row(&self, r: &RawRow) {
        use std::io::Write as _;
        let Ok(mut guard) = self.out.lock() else {
            return;
        };
        let (out, rows) = &mut *guard;
        let period = r
            .period_ns
            .map_or(String::new(), |p| format!("{:.4}", p / 1e6));
        let _ = writeln!(
            out,
            "{},{},{},{:.3},{},{},{},{},{},{},{}",
            r.width,
            r.height,
            r.wake_ns,
            r.wait_ns as f64 / 1e6,
            r.sync_refresh,
            r.sync_ns,
            r.present_count,
            r.present_refresh,
            period,
            r.correction,
            r.clock_refresh
        );
        *rows += 1;
        if *rows % 60 == 0 {
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: f64 = 16_683_333.333; // 59.94 Hz

    #[test]
    fn exact_observations_give_period_and_phase() {
        let mut c = RefreshClock::default();
        for n in 100..130 {
            c.observe_refresh(n, (5_000_000.0 + P * n as f64) as i64);
        }
        assert!((c.period().unwrap() - P).abs() < 1.0);
        let t = c.time_of(1000).unwrap();
        assert!((t as f64 - (5_000_000.0 + P * 1000.0)).abs() < 50.0);
        let (n, _) = c.next_after((5_000_000.0 + P * 200.5) as i64).unwrap();
        assert_eq!(n, 201);
    }

    #[test]
    fn jittery_ticks_still_find_the_refresh_train() {
        let mut c = RefreshClock::default();
        // Loop wake-ups 0..2 ms late, one refresh skipped now and then.
        let mut seed = 7u64;
        let mut n = 0i64;
        for i in 0..200 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let jitter = (seed >> 33) % 2_000_000;
            n += if i % 37 == 0 { 2 } else { 1 };
            c.observe_tick((P * n as f64) as i64 + jitter as i64);
        }
        assert!((c.period().unwrap() - P).abs() < P * 0.001);
        // The phase is biased by the mean wake-up delay (~1 ms), no more.
        let (_, t) = c.next_after((P * 500.2) as i64).unwrap();
        let err = t as f64 - P * 501.0;
        assert!((0.0..2_000_000.0).contains(&err), "error {err}");
    }

    #[test]
    fn the_raw_log_writes_a_header_and_one_line_per_row() {
        let dir = std::env::temp_dir().join(format!("lcplay-vsync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vsync.csv");
        let log = RawLog::create(&path).unwrap();
        let row = RawRow {
            width: 3840,
            height: 2160,
            wake_ns: 1_000_000_000,
            wait_ns: 15_500_000,
            sync_refresh: 42,
            sync_ns: 990_000_000,
            present_count: 40,
            present_refresh: 41,
            clock_refresh: 43,
            period_ns: Some(P),
            correction: 1,
        };
        log.row(&row);
        log.row(&RawRow {
            period_ns: None,
            ..row
        });
        drop(log); // flushes
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], RawLog::HEADER);
        assert_eq!(
            lines[1],
            "3840,2160,1000000000,15.500,42,990000000,40,41,16.6833,1,43"
        );
        assert_eq!(
            lines[2],
            "3840,2160,1000000000,15.500,42,990000000,40,41,,1,43"
        );
        assert_eq!(lines.len(), 3);
        // Every row has as many fields as the header.
        let fields = lines[0].split(',').count();
        assert!(lines.iter().all(|l| l.split(',').count() == fields));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A driver whose counter loses one refresh every `every` reports while
    /// the timestamps stay exact: the period and the numbering must not
    /// notice.
    #[test]
    fn a_counter_that_loses_refreshes_does_not_bend_the_clock() {
        let mut c = RefreshClock::default();
        let mut count = 1000i64;
        // Reports 1 or 3 refreshes apart, as when frames are repeated.
        let mut previous = 1000i64;
        for k in 0..600i64 {
            let refresh = 1000 + k * 2 + (k % 2);
            count += refresh - previous;
            previous = refresh;
            if k % 7 == 6 {
                count -= 1; // the driver loses one
            }
            c.observe_refresh(count, (P * refresh as f64) as i64);
            if k >= MIN_SAMPLES as i64 {
                let p = c.period().unwrap();
                assert!((p - P).abs() < P * 1e-4, "report {k}: period {p}");
            }
        }
        // The clock's numbering follows time, not the driver's count…
        let last = 1000 + 599 * 2 + (599 % 2);
        assert_eq!(c.from_driver(count), last);
        assert!(count < last, "the scenario must lose counts");
        // …and its line still lands on the refreshes.
        let t = c.time_of(last + 10).unwrap();
        assert!((t as f64 - P * (last + 10) as f64).abs() < 100_000.0);
    }

    #[test]
    fn a_new_counter_continues_the_numbering() {
        let mut c = RefreshClock::default();
        for n in 0..30i64 {
            c.observe_refresh(500 + n, (P * n as f64) as i64);
        }
        let before = c.from_driver(529);
        // The swap chain is re-created: its counter starts again near 0.
        c.observe_refresh(3, (P * 32.0) as i64);
        assert_eq!(c.from_driver(3), before + 3, "numbered on by time");
        assert_eq!(c.period(), None, "a new line is fitted");
        for n in 33..50i64 {
            c.observe_refresh(n - 29, (P * n as f64) as i64);
        }
        assert!((c.period().unwrap() - P).abs() < 1.0);
        assert_eq!(c.from_driver(20), before + 20);
    }

    /// Two minutes without a report (a hidden window). Whatever small error
    /// the period has adds up over the gap — here to 0.6 of a refresh, which
    /// time alone would round to the wrong refresh. The driver's count
    /// numbers it instead, and the line is fitted anew.
    #[test]
    fn after_a_long_gap_the_driver_count_numbers_the_refresh() {
        let mut c = RefreshClock::default();
        for n in 0..30i64 {
            c.observe_refresh(n, (P * n as f64) as i64);
        }
        let before = c.from_driver(29);
        let gap = 7200; // refreshes: two minutes at 60 Hz
        let t = (P * (29 + gap) as f64 + 0.6 * P) as i64;
        c.observe_refresh(29 + gap, t);
        assert_eq!(c.from_driver(29 + gap), before + gap);
        assert_eq!(c.period(), None, "a new line is fitted");
        // Reports go on at the display's real rate: a new line, on the
        // driver's numbering.
        for k in 1..20i64 {
            c.observe_refresh(29 + gap + k, t + (P * k as f64) as i64);
        }
        assert!((c.period().unwrap() - P).abs() < 1.0);
        assert_eq!(c.from_driver(29 + gap + 19), before + gap + 19);
    }

    /// Replay of a real capture (`testdata/vsync-laptop-slow.csv`, a Windows
    /// laptop whose driver lost counts while the output ran slowly). By the
    /// driver's own numbering the refreshes end up more than 400 ms off a
    /// 60 Hz train; the clock must hold 60 Hz throughout.
    #[test]
    fn replay_of_a_laptop_capture_holds_sixty_hertz() {
        const NOMINAL: f64 = 1e9 / 60.0;
        let rows: Vec<[i64; 4]> = include_str!("../testdata/vsync-laptop-slow.csv")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("sync_refresh"))
            .map(|l| {
                let v: Vec<i64> = l.split(',').map(|f| f.parse().unwrap()).collect();
                [v[0], v[1], v[2], v[3]]
            })
            .collect();
        assert!(rows.len() > 3000);
        // Where the second swap-chain counter starts.
        let reset = rows.windows(2).position(|w| w[1][0] < w[0][0]).unwrap() + 1;

        // The capture does exercise the fault: the driver's count falls
        // behind the time elapsed.
        let ([c0, t0, ..], [c1, t1, ..]) = (rows[reset], rows[rows.len() - 1]);
        let behind = (t1 - t0) as f64 - (c1 - c0) as f64 * NOMINAL;
        assert!(behind > 400e6, "driver numbering is off by {behind} ns");

        let mut clock = RefreshClock::default();
        let mut worst: f64 = 0.0;
        let mut n_at_reset = None;
        for (i, &[count, t, _, reached]) in rows.iter().enumerate() {
            clock.observe_refresh(count, t);
            if i == reset {
                n_at_reset = Some(clock.from_driver(count));
            }
            if i >= reset + MIN_SAMPLES {
                let p = clock.period().expect("a fit");
                worst = worst.max((p - NOMINAL).abs() / NOMINAL);
                // The refresh a present reached is the report's refresh.
                assert_eq!(clock.from_driver(reached), clock.from_driver(count));
            }
        }
        assert!(worst < 1e-3, "period off by {:.3} %", worst * 100.0);
        // From the reset to the end, the clock counted the refreshes that
        // elapsed, to within one.
        let counted = clock.from_driver(c1) - n_at_reset.unwrap();
        let elapsed = ((t1 - t0) as f64 / NOMINAL).round() as i64;
        assert!((counted - elapsed).abs() <= 1, "{counted} vs {elapsed}");
    }

    #[test]
    fn a_stall_restarts_the_estimate() {
        let mut c = RefreshClock::default();
        for n in 0..20 {
            c.observe_tick((P * n as f64) as i64);
        }
        assert!(c.period().is_some());
        c.observe_tick(5_000_000_000);
        assert!(c.period().is_none());
    }

    #[test]
    fn the_tracker_learns_the_real_queue_depth_and_counts_misses() {
        let mut t = PresentTracker::default();
        // Frames are predicted for refresh n but land on n + 1.
        for id in 0..20u32 {
            let c = t.correction();
            t.presented(id, id as i64, c);
            t.landed(id, id as i64 + 1);
        }
        assert_eq!(t.correction(), 1);
        assert_eq!(t.missed, 0);
        // One lands two refreshes late: a miss.
        t.presented(20, 20, 1);
        t.landed(20, 22);
        assert_eq!(t.missed, 1);
    }

    #[test]
    fn the_correction_does_not_feed_back_on_itself() {
        // Closed loop, as in the render loop: the correction in force is
        // applied, and frames really land one refresh after the uncorrected
        // prediction every time. The correction must settle at 1 and stay,
        // with no misses.
        let mut t = PresentTracker::default();
        let mut used = Vec::new();
        for id in 0..200u32 {
            let base = id as i64;
            let c = t.correction();
            used.push(c);
            t.presented(id, base, c);
            t.landed(id, base + 1);
        }
        assert!(used[20..].iter().all(|&c| c == 1), "{:?}", &used[..40]);
        assert_eq!(t.missed, 0);
    }

    #[test]
    fn present_ids_wrap() {
        let mut t = PresentTracker::default();
        for k in 0..12u32 {
            let id = (u32::MAX - 5).wrapping_add(k);
            t.presented(id, k as i64, 0);
            t.landed(id, k as i64);
        }
        assert_eq!(t.correction(), 0);
        assert!(t.pending.is_empty());
    }
}
