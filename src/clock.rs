//! The **master show clock**.
//!
//! Every output and every layer reads playback position from this one clock, so
//! all outputs stay in lockstep by construction (they share a process and a
//! frame loop). Phase 3 will extend this with a network time source for
//! multi-machine sync; the interface stays the same.

use std::time::Instant;

#[derive(Debug)]
pub struct Clock {
    /// Accumulated playback time (seconds) up to the last pause.
    base: f64,
    /// When the current play segment started. `None` while paused.
    running_since: Option<Instant>,
    /// Loop length in seconds, if looping is enabled.
    loop_len: Option<f64>,
}

impl Clock {
    pub fn new(autoplay: bool, loop_len: Option<f64>) -> Self {
        Clock {
            base: 0.0,
            running_since: autoplay.then(Instant::now),
            loop_len: loop_len.filter(|&d| d > 0.0),
        }
    }

    /// Current show time in seconds, wrapped to the loop length if set.
    pub fn now(&self) -> f64 {
        let raw = match self.running_since {
            Some(start) => self.base + start.elapsed().as_secs_f64(),
            None => self.base,
        };
        match self.loop_len {
            Some(len) => raw.rem_euclid(len),
            None => raw,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.running_since.is_some()
    }

    pub fn play(&mut self) {
        if self.running_since.is_none() {
            self.running_since = Some(Instant::now());
        }
    }

    pub fn pause(&mut self) {
        if let Some(start) = self.running_since.take() {
            self.base += start.elapsed().as_secs_f64();
        }
    }

    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Jump the clock by a relative number of seconds (can be negative),
    /// clamping at zero. Keeps the play/pause state intact.
    pub fn seek_by(&mut self, delta: f64) {
        let target = (self.now() + delta).max(0.0);
        self.seek_to(target);
    }

    /// Jump the clock to an absolute time in seconds.
    pub fn seek_to(&mut self, t: f64) {
        self.base = t.max(0.0);
        if self.running_since.is_some() {
            self.running_since = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_clock_does_not_advance() {
        let mut c = Clock::new(false, None);
        assert_eq!(c.now(), 0.0);
        c.seek_to(10.0);
        assert!((c.now() - 10.0).abs() < 1e-6);
        assert!(!c.is_playing());
    }

    #[test]
    fn looping_wraps() {
        let mut c = Clock::new(false, Some(5.0));
        c.seek_to(7.5);
        assert!((c.now() - 2.5).abs() < 1e-6);
    }

    #[test]
    fn toggle_flips_state() {
        let mut c = Clock::new(false, None);
        assert!(!c.is_playing());
        c.toggle();
        assert!(c.is_playing());
        c.toggle();
        assert!(!c.is_playing());
    }
}
