//! Frame timing log: where the time went when a frame took too long.
//!
//! The render loop is one thread doing, once per frame:
//!
//! ```text
//!   ┌ outside ─┐┌ housekeeping ┐┌ acquire ┐┌ update ┐┌ render ┐┌ present ┐
//!   winit events  operator      wait for    layers:   record +  hand the
//!   (window       requests,     the swap    decode     submit    frame to
//!   messages)     show edits,   chain       results,   the GPU   the swap
//!                 reloads       (vsync)     uploads    work      chain
//! ```
//!
//! Normally the frame interval is one refresh and nearly all of it is spent
//! in *acquire*, waiting for the display. When an interval is longer than the
//! threshold (by default two refreshes, i.e. at least one refresh with no new
//! frame), one line is logged with the time of each phase, the slowest layer
//! and what else happened in that frame (a show rebuild, a preview capture…).
//! That says which part of the Runner to look at:
//!
//! * **acquire** long — the GPU or the presentation path is behind (the
//!   frames queued earlier have not been shown yet);
//! * **update** long — a layer's content (the slowest one is named): texture
//!   uploads, a decoder being stopped or started;
//! * **housekeeping** long — a show edit, an asset swap or a reload rebuilt
//!   things on this thread;
//! * **outside** long — the window system held the loop (window messages,
//!   moving or resizing a window);
//! * **render** / **present** long — submitting to the GPU or presenting
//!   blocked in the driver. (Texture uploads are copied into staging memory
//!   during *update*; the copy to the GPU itself is part of the submission
//!   in *render*.)
//!
//! The log is rate-limited (four lines a second, but a frame of 100 ms or
//! more is always logged); the periodic frame-rate line also gives the
//! slowest frame and how many were over the threshold.
//!
//! `LCPLAY_FRAME_LOG`: unset = on, threshold two refreshes; `0` or `off` =
//! no log; a number = threshold in milliseconds.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// The parts of one render-loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Operator requests, show edits, asset swaps, reloads.
    Housekeeping,
    /// Waiting for each output's swap chain to take a frame.
    Acquire,
    /// Bringing the layers' contents to the frame (uploads included).
    Update,
    /// Recording and submitting the GPU work.
    Render,
    /// Presenting.
    Present,
}

const PHASES: [(Phase, &str); 5] = [
    (Phase::Housekeeping, "housekeeping"),
    (Phase::Acquire, "acquire"),
    (Phase::Update, "update"),
    (Phase::Render, "render"),
    (Phase::Present, "present"),
];

/// Detailed lines logged per second at most; more are only counted…
const MAX_LINES_PER_SECOND: u32 = 4;
/// …except frames at least this long: the hiccups the log is for, never
/// hidden behind a run of slightly slow ones (and at most ten a second by
/// their very length).
const ALWAYS_LOGGED: Duration = Duration::from_millis(100);

/// Used for the threshold until an output knows its refresh period.
const FALLBACK_PERIOD: Duration = Duration::from_micros(16_667);

/// How slow frames are recognised.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Threshold {
    Off,
    /// A multiple of the refresh period.
    Refreshes(f64),
    Fixed(Duration),
}

impl Threshold {
    fn parse(value: Option<&str>) -> Threshold {
        match value.map(str::trim) {
            None | Some("") => Threshold::Refreshes(2.0),
            Some(v) if v == "0" || v.eq_ignore_ascii_case("off") => Threshold::Off,
            Some(v) => match v.parse::<f64>() {
                Ok(ms) if ms > 0.0 && ms.is_finite() => {
                    Threshold::Fixed(Duration::from_secs_f64(ms / 1000.0))
                }
                _ => {
                    tracing::warn!(
                        "LCPLAY_FRAME_LOG='{v}' is not a number of milliseconds, 0 or off: \
                         using two refreshes"
                    );
                    Threshold::Refreshes(2.0)
                }
            },
        }
    }

    fn of(self, period: Duration) -> Option<Duration> {
        match self {
            Threshold::Off => None,
            Threshold::Refreshes(n) => Some(period.mul_f64(n)),
            Threshold::Fixed(d) => Some(d),
        }
    }
}

/// Measures each render-loop iteration and reports the slow ones.
pub struct FrameTimer {
    threshold: Threshold,
    /// When the previous iteration ended (`None`: no frame rendered yet, or
    /// the loop was not rendering).
    last_end: Option<Instant>,
    /// When this iteration started.
    began: Option<Instant>,
    spent: [Duration; PHASES.len()],
    /// The layer whose update took longest this frame: (index, time).
    slowest_layer: Option<(usize, Duration)>,
    /// What else happened this frame ("show rebuilt"…).
    notes: Vec<&'static str>,
    // Rate limit.
    window_start: Option<Instant>,
    lines_in_window: u32,
    // Since the last summary.
    worst: Duration,
    slow: u32,
    unlogged: u32,
    last_threshold: Option<Duration>,
}

impl FrameTimer {
    /// Configured from `LCPLAY_FRAME_LOG`.
    pub fn from_env() -> Self {
        let threshold = Threshold::parse(std::env::var("LCPLAY_FRAME_LOG").ok().as_deref());
        match threshold {
            Threshold::Off => tracing::info!("frame timing log off (LCPLAY_FRAME_LOG)"),
            Threshold::Fixed(d) => tracing::info!(
                "frame timing log: frames longer than {:.1} ms",
                d.as_secs_f64() * 1e3
            ),
            Threshold::Refreshes(_) => {}
        }
        Self::with(threshold)
    }

    fn with(threshold: Threshold) -> Self {
        FrameTimer {
            threshold,
            last_end: None,
            began: None,
            spent: [Duration::ZERO; PHASES.len()],
            slowest_layer: None,
            notes: Vec::new(),
            window_start: None,
            lines_in_window: 0,
            worst: Duration::ZERO,
            slow: 0,
            unlogged: 0,
            last_threshold: None,
        }
    }

    /// Whether anything is measured at all.
    pub fn enabled(&self) -> bool {
        self.threshold != Threshold::Off
    }

    /// An iteration of the render loop starts.
    pub fn begin(&mut self) {
        self.begin_at(Instant::now());
    }

    fn begin_at(&mut self, now: Instant) {
        self.began = Some(now);
        self.spent = [Duration::ZERO; PHASES.len()];
        self.slowest_layer = None;
        self.notes.clear();
    }

    /// Time spent in `phase` (added up when a phase occurs several times in
    /// a frame, e.g. once per graphics card).
    pub fn add(&mut self, phase: Phase, d: Duration) {
        if let Some(i) = PHASES.iter().position(|(p, _)| *p == phase) {
            self.spent[i] += d;
        }
    }

    /// Time `phase` from `since` until now.
    pub fn since(&mut self, phase: Phase, since: Instant) {
        self.add(phase, since.elapsed());
    }

    /// Layer `index` took `d` to update.
    pub fn layer(&mut self, index: usize, d: Duration) {
        if self.slowest_layer.is_none_or(|(_, worst)| d > worst) {
            self.slowest_layer = Some((index, d));
        }
    }

    /// Something worth knowing happened in this frame.
    pub fn note(&mut self, what: &'static str) {
        if self.enabled() && !self.notes.contains(&what) {
            self.notes.push(what);
        }
    }

    /// This iteration rendered nothing: the next one starts afresh (its
    /// interval would otherwise include the idle time).
    pub fn skip(&mut self) {
        self.last_end = None;
        self.began = None;
    }

    /// A frame was rendered. `period` is the shortest refresh period of the
    /// outputs (if known); `label` names a layer by its index. Returns the
    /// line to log if this frame was slow (and the rate limit allows it).
    pub fn end(
        &mut self,
        period: Option<Duration>,
        label: impl FnOnce(usize) -> String,
    ) -> Option<String> {
        self.end_at(Instant::now(), period, label)
    }

    fn end_at(
        &mut self,
        now: Instant,
        period: Option<Duration>,
        label: impl FnOnce(usize) -> String,
    ) -> Option<String> {
        let period = period.unwrap_or(FALLBACK_PERIOD);
        let threshold = self.threshold.of(period)?;
        let (Some(last_end), Some(began)) = (self.last_end.replace(now), self.began) else {
            return None;
        };
        let interval = now.saturating_duration_since(last_end);
        self.worst = self.worst.max(interval);
        self.last_threshold = Some(threshold);
        if interval <= threshold {
            return None;
        }
        self.slow += 1;

        // Rate limit: a run of slow frames must not turn the log (written by
        // this very thread) into a cause of more of them.
        let window = self.window_start.get_or_insert(now);
        if now.saturating_duration_since(*window) >= Duration::from_secs(1) {
            *window = now;
            self.lines_in_window = 0;
        }
        if self.lines_in_window >= MAX_LINES_PER_SECOND && interval < ALWAYS_LOGGED {
            self.unlogged += 1;
            return None;
        }
        self.lines_in_window += 1;

        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let outside = began.saturating_duration_since(last_end);
        let measured: Duration = self.spent.iter().sum::<Duration>() + outside;
        let other = interval.saturating_sub(measured);
        let mut line = format!(
            "slow frame: {:.1} ms (refresh {:.1} ms, threshold {:.1} ms) — outside {:.1}",
            ms(interval),
            ms(period),
            ms(threshold),
            ms(outside)
        );
        let slowest = self
            .slowest_layer
            .map(|(index, d)| format!(" (slowest {} {:.1})", label(index), ms(d)));
        for ((phase, name), spent) in PHASES.iter().zip(self.spent) {
            let _ = write!(line, ", {name} {:.1}", ms(spent));
            if *phase == Phase::Update {
                line.push_str(slowest.as_deref().unwrap_or_default());
            }
        }
        let _ = write!(line, ", other {:.1} ms", ms(other));
        if !self.notes.is_empty() {
            let _ = write!(line, " [{}]", self.notes.join(", "));
        }
        Some(line)
    }

    /// For the periodic frame-rate line: the slowest frame since the last
    /// summary and how many were over the threshold. Resets the counts.
    pub fn summary(&mut self) -> Option<String> {
        let threshold = self.last_threshold?;
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let mut text = format!(
            "slowest frame {:.1} ms, {} over {:.1} ms",
            ms(self.worst),
            self.slow,
            ms(threshold)
        );
        if self.unlogged > 0 {
            let _ = write!(text, " ({} not logged)", self.unlogged);
        }
        self.worst = Duration::ZERO;
        self.slow = 0;
        self.unlogged = 0;
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const PERIOD: Option<Duration> = Some(Duration::from_micros(16_667));

    /// One frame: `outside` before it begins, then the given phases.
    fn frame(
        t: &mut FrameTimer,
        at: &mut Instant,
        outside: Duration,
        phases: &[(Phase, Duration)],
    ) -> Option<String> {
        *at += outside;
        t.begin_at(*at);
        for &(phase, d) in phases {
            t.add(phase, d);
            *at += d;
        }
        t.end_at(*at, PERIOD, |i| format!("layer{i}"))
    }

    #[test]
    fn the_threshold_comes_from_the_environment_value() {
        assert_eq!(Threshold::parse(None), Threshold::Refreshes(2.0));
        assert_eq!(Threshold::parse(Some("")), Threshold::Refreshes(2.0));
        assert_eq!(Threshold::parse(Some("0")), Threshold::Off);
        assert_eq!(Threshold::parse(Some("OFF")), Threshold::Off);
        assert_eq!(Threshold::parse(Some("25")), Threshold::Fixed(25 * MS));
        assert_eq!(Threshold::parse(Some("-3")), Threshold::Refreshes(2.0));
        assert_eq!(Threshold::parse(Some("abc")), Threshold::Refreshes(2.0));
    }

    #[test]
    fn normal_frames_are_quiet_and_slow_ones_are_broken_down() {
        let mut t = FrameTimer::with(Threshold::Refreshes(2.0));
        let mut at = Instant::now();
        // The first frame only starts the measurement.
        assert_eq!(
            frame(&mut t, &mut at, MS, &[(Phase::Acquire, 15 * MS)]),
            None
        );
        // A normal frame: one refresh, mostly waiting for the swap chain.
        assert_eq!(
            frame(
                &mut t,
                &mut at,
                MS,
                &[(Phase::Acquire, 14 * MS), (Phase::Update, MS)]
            ),
            None
        );
        // A stalled update.
        let line = {
            at += MS;
            t.begin_at(at);
            t.note("show rebuilt");
            t.add(Phase::Acquire, 2 * MS);
            t.add(Phase::Update, 80 * MS);
            t.layer(0, 5 * MS);
            t.layer(3, 70 * MS);
            t.layer(1, 5 * MS);
            at += 90 * MS; // 82 measured + 8 unaccounted for
            t.end_at(at, PERIOD, |i| format!("'layer{i}'"))
        }
        .expect("a slow frame is logged");
        assert!(line.starts_with("slow frame: 91.0 ms"), "{line}");
        assert!(line.contains("outside 1.0"), "{line}");
        assert!(
            line.contains("update 80.0 (slowest 'layer3' 70.0)"),
            "{line}"
        );
        assert!(line.contains("other 8.0 ms"), "{line}");
        assert!(line.ends_with("[show rebuilt]"), "{line}");

        let summary = t.summary().unwrap();
        assert!(
            summary.starts_with("slowest frame 91.0 ms, 1 over 33.3 ms"),
            "{summary}"
        );
        // Counts start again after a summary.
        assert!(t
            .summary()
            .unwrap()
            .starts_with("slowest frame 0.0 ms, 0 over"));
    }

    #[test]
    fn idle_time_is_not_a_slow_frame() {
        let mut t = FrameTimer::with(Threshold::Refreshes(2.0));
        let mut at = Instant::now();
        frame(&mut t, &mut at, MS, &[(Phase::Acquire, 15 * MS)]);
        // No outputs for a while.
        t.skip();
        at += Duration::from_secs(2);
        assert_eq!(
            frame(&mut t, &mut at, MS, &[(Phase::Acquire, 15 * MS)]),
            None
        );
        assert_eq!(
            frame(&mut t, &mut at, MS, &[(Phase::Acquire, 15 * MS)]),
            None
        );
    }

    #[test]
    fn a_run_of_slow_frames_is_rate_limited_and_counted() {
        let mut t = FrameTimer::with(Threshold::Fixed(20 * MS));
        let mut at = Instant::now();
        frame(&mut t, &mut at, MS, &[(Phase::Acquire, 15 * MS)]);
        let logged = (0..10)
            .filter_map(|_| frame(&mut t, &mut at, MS, &[(Phase::Render, 50 * MS)]))
            .count();
        // Ten frames of 51 ms: about half a second, one window.
        assert_eq!(logged, MAX_LINES_PER_SECOND as usize);
        let summary = t.summary().unwrap();
        assert!(summary.contains("10 over 20.0 ms"), "{summary}");
        assert!(summary.contains("(6 not logged)"), "{summary}");
        // Over the limit, a real hiccup is still logged.
        let big = frame(&mut t, &mut at, MS, &[(Phase::Update, 250 * MS)]);
        assert!(big.is_some_and(|l| l.starts_with("slow frame: 251.0 ms")));
    }

    #[test]
    fn off_measures_nothing() {
        let mut t = FrameTimer::with(Threshold::Off);
        let mut at = Instant::now();
        assert!(!t.enabled());
        for _ in 0..3 {
            assert_eq!(
                frame(&mut t, &mut at, MS, &[(Phase::Update, 500 * MS)]),
                None
            );
        }
        assert_eq!(t.summary(), None);
    }
}
