//! Deterministic elementary functions for the starter-asset synthesizer.
//!
//! The platform `libm` (`f64::sin`, `f64::exp`, …) may differ in the last bit between operating systems
//! and library versions, which would make `soniccraft init` write slightly different WAV files on
//! different machines. These implementations use only IEEE 754 basic operations (`+ − × ÷`, `sqrt`,
//! `floor`/`round`), which are correctly rounded everywhere and never fused by rustc, so the generated
//! assets are bit-identical on every platform. Accuracy (~1e-13) is far beyond what 16-bit audio needs.

use std::f64::consts::{FRAC_PI_2, LN_2, PI, TAU};

/// `1 / ((2k)(2k+1))` for k = 1..=8: the Taylor recurrence of sin (terms up to x^17).
const SIN_STEPS: [f64; 8] = [
    1.0 / (2.0 * 3.0),
    1.0 / (4.0 * 5.0),
    1.0 / (6.0 * 7.0),
    1.0 / (8.0 * 9.0),
    1.0 / (10.0 * 11.0),
    1.0 / (12.0 * 13.0),
    1.0 / (14.0 * 15.0),
    1.0 / (16.0 * 17.0),
];

/// `1 / n` for n = 1..=14: the Taylor recurrence of exp.
const EXP_STEPS: [f64; 14] = [
    1.0,
    1.0 / 2.0,
    1.0 / 3.0,
    1.0 / 4.0,
    1.0 / 5.0,
    1.0 / 6.0,
    1.0 / 7.0,
    1.0 / 8.0,
    1.0 / 9.0,
    1.0 / 10.0,
    1.0 / 11.0,
    1.0 / 12.0,
    1.0 / 13.0,
    1.0 / 14.0,
];

/// `sin(2π · turns)`. Reducing in turns first keeps long notes (large phases) accurate.
pub(crate) fn sin_turns(turns: f64) -> f64 {
    if !turns.is_finite() {
        return 0.0;
    }
    let frac = turns - turns.floor(); // [0, 1)
    let centered = if frac >= 0.5 { frac - 1.0 } else { frac }; // [-0.5, 0.5)
    sin_reduced(centered * TAU)
}

/// `cos(2π · turns)`.
pub(crate) fn cos_turns(turns: f64) -> f64 {
    sin_turns(turns + 0.25)
}

/// sin(x) for x in [-π, π].
fn sin_reduced(x: f64) -> f64 {
    let r = if x > FRAC_PI_2 {
        PI - x
    } else if x < -FRAC_PI_2 {
        -PI - x
    } else {
        x
    };
    let r2 = r * r;
    // sin r = r (1 − r²/6 (1 − r²/20 (1 − …))): Horner form of the Taylor series, |error| < 5e-14.
    let mut p = 1.0;
    for step in SIN_STEPS.iter().rev() {
        p = 1.0 - r2 * step * p;
    }
    r * p
}

/// `e^x` (0 below −708, +∞ above 709).
pub(crate) fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x < -708.0 {
        return 0.0;
    }
    if x > 709.0 {
        return f64::INFINITY;
    }
    // x = k·ln2 + r with |r| ≤ ln2/2, e^x = 2^k · e^r.
    let k = (x / LN_2).round();
    let r = x - k * LN_2;
    let mut p = 1.0;
    for step in EXP_STEPS.iter().rev() {
        p = 1.0 + r * step * p;
    }
    p * pow2(k as i32)
}

/// 2^k, exact, for k in [-1074, 1023] (0 below, +∞ above).
fn pow2(k: i32) -> f64 {
    if k > 1023 {
        f64::INFINITY
    } else if k >= -1022 {
        f64::from_bits(((k + 1023) as u64) << 52)
    } else if k >= -1074 {
        f64::from_bits(1u64 << (k + 1074))
    } else {
        0.0
    }
}

/// Hyperbolic tangent.
pub(crate) fn tanh(x: f64) -> f64 {
    if x > 19.0 {
        return 1.0;
    }
    if x < -19.0 {
        return -1.0;
    }
    let e = exp(2.0 * x);
    (e - 1.0) / (e + 1.0)
}

/// Smoothstep `3s² − 2s³` of `s` clamped to [0, 1]: exactly 0 at 0 and exactly 1 at 1.
pub(crate) fn smoothstep(s: f64) -> f64 {
    let s = s.clamp(0.0, 1.0);
    s * s * (3.0 - 2.0 * s)
}

/// SplitMix64: a tiny, fixed-seed, platform-independent PRNG for noise.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator with `seed`.
    pub(crate) fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    /// The next 64 random bits.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [-1, 1).
    pub(crate) fn bipolar(&mut self) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
        2.0 * unit - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_matches_libm() {
        let mut worst: f64 = 0.0;
        for i in -20_000..=20_000 {
            let turns = i as f64 * 0.000_731;
            worst = worst.max((sin_turns(turns) - (turns * TAU).sin()).abs());
            worst = worst.max((cos_turns(turns) - (turns * TAU).cos()).abs());
        }
        assert!(worst < 1e-12, "worst error {worst}");
        // large phases stay accurate thanks to the reduction in turns
        assert!((sin_turns(40_000.125) - (0.125 * TAU).sin()).abs() < 1e-9);
        assert_eq!(sin_turns(0.0), 0.0);
    }

    #[test]
    fn exp_and_tanh_match_libm() {
        let mut worst: f64 = 0.0;
        for i in -4_000..=1_000 {
            let x = i as f64 * 0.01;
            let rel = (exp(x) - x.exp()).abs() / x.exp();
            worst = worst.max(rel);
            assert!((tanh(x) - x.tanh()).abs() < 1e-13, "tanh({x})");
        }
        assert!(worst < 1e-14, "worst relative error {worst}");
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(exp(-800.0), 0.0);
        assert_eq!(tanh(0.0), 0.0);
        assert_eq!(tanh(25.0), 1.0);
    }

    #[test]
    fn smoothstep_edges_are_exact() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(-3.0), 0.0);
        assert_eq!(smoothstep(0.5), 0.5);
    }

    #[test]
    fn prng_is_fixed() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..1000 {
            let x = a.bipolar();
            assert_eq!(x, b.bipolar());
            assert!((-1.0..1.0).contains(&x));
        }
        // SplitMix64 reference value for seed 0.
        assert_eq!(SplitMix64::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }
}
