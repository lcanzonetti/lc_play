//! Band-limited interpolation: reading a stream of samples at fractional
//! positions, for the output's resampler.
//!
//! The mixer produces frames on the *cluster* sample grid (frame *g* is
//! the sound due at cluster time *g* / 48 kHz). The sound card consumes
//! frames on its *own* clock, which runs a few tens of ppm fast or slow —
//! and possibly at another nominal rate (an ASIO card set to 44.1 or
//! 96 kHz). The resampler reads the grid at the positions the card's
//! frames correspond to.
//!
//! Classic polyphase windowed sinc: [`TAPS`] input frames per output frame,
//! a Kaiser window, the impulse response tabulated at [`PHASES`] fractional
//! positions and interpolated linearly between them. It is symmetric, so a
//! position *p* yields the signal *at p* — no group delay to account for,
//! only a look-ahead of `TAPS / 2` frames, which the mixer provides (it can
//! compute any frame of the future). Each phase's coefficients sum to one,
//! so a constant comes out unchanged.
//!
//! Cost: `TAPS` multiply-adds per channel per output frame — for 16
//! channels at 48 kHz, 25 million a second, a few milliseconds of one core.

/// Input frames each output frame is computed from.
pub const TAPS: usize = 32;
/// Frames needed before and after a position: `floor(p) - HALF + 1 ..=
/// floor(p) + HALF`.
pub const HALF: i64 = (TAPS / 2) as i64;
/// Fractional positions tabulated.
const PHASES: usize = 512;
/// Kaiser window shape: about 80 dB of stop-band attenuation.
const BETA: f64 = 8.0;

pub struct Sinc {
    /// `(PHASES + 1) × TAPS` coefficients: row `j` is for the fractional
    /// position `j / PHASES`.
    table: Vec<f32>,
}

impl Sinc {
    /// `ratio`: output rate over input rate. The pass band ends a little
    /// below the lower of the two Nyquist frequencies, so downsampling does
    /// not alias.
    pub fn new(ratio: f64) -> Sinc {
        let cutoff = 0.92 * ratio.clamp(0.05, 1.0);
        let i0_beta = bessel_i0(BETA);
        let mut table = vec![0f32; (PHASES + 1) * TAPS];
        for j in 0..=PHASES {
            let frac = j as f64 / PHASES as f64;
            let row = &mut table[j * TAPS..(j + 1) * TAPS];
            let mut sum = 0.0;
            let mut taps = [0f64; TAPS];
            for (k, tap) in taps.iter_mut().enumerate() {
                // Distance from the position to input frame
                // `floor(p) - HALF + 1 + k`.
                let d = (k as f64 - HALF as f64 + 1.0) - frac;
                let x = d / HALF as f64;
                let window = if x.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(BETA * (1.0 - x * x).sqrt()) / i0_beta
                };
                let arg = std::f64::consts::PI * cutoff * d;
                let sinc = if arg.abs() < 1e-12 {
                    1.0
                } else {
                    arg.sin() / arg
                };
                *tap = cutoff * sinc * window;
                sum += *tap;
            }
            for (out, tap) in row.iter_mut().zip(taps) {
                *out = (tap / sum) as f32;
            }
        }
        Sinc { table }
    }

    /// The coefficients for fractional position `frac` (0 ≤ frac < 1),
    /// applied to input frames `floor(p) - HALF + 1 ..= floor(p) + HALF`.
    #[inline]
    pub fn weights(&self, frac: f64, out: &mut [f32; TAPS]) {
        let x = frac.clamp(0.0, 1.0) * PHASES as f64;
        let j = (x as usize).min(PHASES - 1);
        let t = (x - j as f64) as f32;
        let a = &self.table[j * TAPS..(j + 1) * TAPS];
        let b = &self.table[(j + 1) * TAPS..(j + 2) * TAPS];
        for k in 0..TAPS {
            out[k] = a[k] + (b[k] - a[k]) * t;
        }
    }
}

/// The zeroth-order modified Bessel function of the first kind (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..64 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read `signal` at position `p` (in frames).
    fn read(sinc: &Sinc, signal: &dyn Fn(i64) -> f64, p: f64) -> f64 {
        let i0 = p.floor() as i64;
        let mut w = [0f32; TAPS];
        sinc.weights(p - i0 as f64, &mut w);
        (0..TAPS)
            .map(|k| w[k] as f64 * signal(i0 - HALF + 1 + k as i64))
            .sum()
    }

    #[test]
    fn a_constant_passes_unchanged_and_integer_positions_are_exact() {
        let s = Sinc::new(1.0);
        for frac in [0.0, 0.1, 0.5, 0.77, 0.999] {
            assert!((read(&s, &|_| 0.5, 10.0 + frac) - 0.5).abs() < 1e-6);
        }
        let ramp = |i: i64| (i as f64 * 0.37).sin();
        assert!((read(&s, &ramp, 40.0) - ramp(40)).abs() < 2e-3);
    }

    #[test]
    fn a_tone_is_reconstructed_between_samples() {
        // 1 kHz and 10 kHz at 48 kHz, read half way between samples.
        let s = Sinc::new(1.0);
        for hz in [1000.0, 10_000.0] {
            let w = 2.0 * std::f64::consts::PI * hz / 48_000.0;
            let tone = move |i: i64| (w * i as f64).sin();
            let mut worst = 0f64;
            for n in 0..200 {
                let p = 1000.0 + n as f64 * 0.731;
                worst = worst.max((read(&s, &tone, p) - (w * p).sin()).abs());
            }
            assert!(worst < 2e-3, "{hz} Hz: error {worst}");
        }
    }
}
