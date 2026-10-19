//! Float → integer PCM quantization with optional TPDF dither from a fixed-seed xorshift PRNG
//! (DESIGN.md §4.8). Shared by the WAV and FLAC encoders so both produce identical integer samples.

/// Fixed seed: every encode of the same buffer produces the same dither sequence.
const DITHER_SEED: u64 = 0x5EED_50D1_C0FF_EE01;

/// xorshift64 (Marsaglia). Small, fast, deterministic and plenty for dither noise.
#[derive(Debug, Clone)]
pub(crate) struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// A generator with the given non-zero seed.
    pub(crate) fn new(seed: u64) -> Self {
        XorShift64 {
            state: if seed == 0 { DITHER_SEED } else { seed },
        }
    }

    /// Next raw 64-bit value.
    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Uniform in `[0, 1)`, with 24 bits of resolution (exactly representable in f32).
    pub(crate) fn next_unit_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 * (1.0 / (1u32 << 24) as f32)
    }
}

/// Quantizes f32 samples to signed integers of a given bit depth.
///
/// `x` is clamped to `[-1, 1)`, scaled by `2^(bits-1)` (the scale symphonia and ffmpeg use for
/// decoding, so integer round trips are exact), optionally offset by TPDF dither of ±1 LSB peak, rounded
/// to nearest and clamped to the integer range. Exact digital silence (`0.0`) is never dithered so that
/// silent passages stay bit-exact zero. Non-finite input maps to 0.
#[derive(Debug, Clone)]
pub(crate) struct Quantizer {
    scale: f32,
    max: f32,
    rng: Option<XorShift64>,
}

impl Quantizer {
    /// A quantizer for `bits` (8..=32) with TPDF dither when `dither` is true.
    pub(crate) fn new(bits: u32, dither: bool) -> Self {
        debug_assert!((8..=32).contains(&bits));
        let scale = (1u64 << (bits - 1)) as f32;
        Quantizer {
            scale,
            max: scale - 1.0,
            rng: dither.then(|| XorShift64::new(DITHER_SEED)),
        }
    }

    /// Quantize one sample.
    pub(crate) fn quantize(&mut self, x: f32) -> i32 {
        if !x.is_finite() || x == 0.0 {
            return 0;
        }
        let mut v = x.clamp(-1.0, 1.0) * self.scale;
        if let Some(rng) = self.rng.as_mut() {
            // Triangular PDF on (-1, 1) LSB: difference of two independent uniforms.
            v += rng.next_unit_f32() - rng.next_unit_f32();
        }
        // f32 → i32 `as` saturates; the clamp keeps the result inside the target bit depth.
        v.round().clamp(-self.scale, self.max) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_and_clamping() {
        let mut q = Quantizer::new(16, false);
        assert_eq!(q.quantize(0.0), 0);
        assert_eq!(q.quantize(1.0), 32767);
        assert_eq!(q.quantize(2.0), 32767);
        assert_eq!(q.quantize(-1.0), -32768);
        assert_eq!(q.quantize(-3.0), -32768);
        assert_eq!(q.quantize(0.5), 16384);
        assert_eq!(q.quantize(1.4 / 32768.0), 1);
        assert_eq!(q.quantize(1.6 / 32768.0), 2);
        assert_eq!(q.quantize(f32::NAN), 0);
        assert_eq!(q.quantize(f32::INFINITY), 0);
        let mut q24 = Quantizer::new(24, false);
        assert_eq!(q24.quantize(1.0), 8_388_607);
        assert_eq!(q24.quantize(-1.0), -8_388_608);
    }

    #[test]
    fn dither_is_deterministic_bounded_and_unbiased() {
        let mut a = Quantizer::new(16, true);
        let mut b = Quantizer::new(16, true);
        let x = 0.25f32;
        let exact = x * 32768.0;
        let mut sum = 0.0f64;
        let n = 100_000;
        for _ in 0..n {
            let qa = a.quantize(x);
            assert_eq!(qa, b.quantize(x));
            assert!((qa as f32 - exact).abs() <= 1.0, "within ±1 LSB");
            sum += qa as f64;
        }
        let mean = sum / n as f64;
        assert!(
            (mean - exact as f64).abs() < 0.01,
            "TPDF dither is unbiased: {mean}"
        );
        // Silence stays silent.
        assert_eq!(a.quantize(0.0), 0);
    }

    #[test]
    fn xorshift_unit_range() {
        let mut r = XorShift64::new(1);
        for _ in 0..10_000 {
            let u = r.next_unit_f32();
            assert!((0.0..1.0).contains(&u));
        }
    }
}
