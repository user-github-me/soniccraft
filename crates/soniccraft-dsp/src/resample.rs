//! Sample-rate conversion (DESIGN.md §4.1 step 2).
//!
//! Uses rubato's synchronous FFT resampler (a windowed-sinc low-pass applied in the frequency domain,
//! Blackman-Harris² window, ~1024-frame blocks) over the whole clip with
//! `process_all_into_buffer`, which pumps the zero-padded tail and removes the resampler's startup
//! delay.
//!
//! Two details make the result sample-accurate:
//!
//! * **Exact alignment.** rubato trims `fft_size_out / 2` frames, which equals the filter's true delay
//!   only when both FFT block sizes are even (the sinc is centred at `fft_size_in / 2`, computed with
//!   integer division). The block count is therefore rounded up to an even number; then an event at
//!   input time `t` appears at output time `t` (no residual sub-sample offset).
//! * **Exact length.** rubato sizes its output with an f64 `ceil`, which can be one frame long; the
//!   output is cut to [`output_len`] = `round(in_frames × out_rate / in_rate)` computed in integers.
//!
//! # Awkward ratios
//!
//! The FFT blocks must hold a whole number of `in_rate / gcd` input and `out_rate / gcd` output frames,
//! so a rate like 44 101 Hz (prime) needs blocks of 88 202 frames, and multi-megahertz rates needed
//! gigabytes. When a block would exceed [`MAX_FFT_BLOCK`] frames, a direct windowed-sinc resampler is
//! used instead: every output frame `m` sits at the exact rational input position `m × in / out`
//! (integer arithmetic, so there is no delay and no drift), and its value is the input convolved with a
//! Kaiser-windowed sinc (β = 10, ≈ 100 dB stopband, passband to 90 % of the lower Nyquist frequency,
//! normalized to unity DC gain at every position) read from a fixed-size table. Memory is independent
//! of the rates; the output length is the same [`output_len`].
//!
//! # Robustness
//!
//! Rates outside [`MIN_SAMPLE_RATE`]–[`MAX_SAMPLE_RATE`] are rejected. Non-finite samples (NaN, ±∞) are
//! resampled as silence and absurd magnitudes (beyond ±10³⁰, i.e. +600 dBFS) are clamped, so the FFT
//! never sees values it would turn into NaN; a panic inside rubato is turned into
//! [`DspError::Resample`]. Decoders already replace non-finite samples (and count them), so this is a
//! last line of defence.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler, WindowFunction};

pub use crate::decode::{MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};
use crate::{AudioBuffer, DspError};

/// Desired FFT block length on the input side (the filter length in input samples).
const TARGET_BLOCK_IN: usize = 1024;

/// Largest FFT block (input or output side, in frames) of the synchronous resampler; ratios that need
/// larger blocks use the direct sinc resampler (see the module docs).
pub const MAX_FFT_BLOCK: usize = 1 << 17;

/// Samples beyond this magnitude are clamped before resampling (see the module docs).
const MAX_SANE_MAGNITUDE: f32 = 1e30;

/// Exact output length of a resample: `round(in_frames × out_rate / in_rate)` (halves round up),
/// computed with integer arithmetic. Returns 0 when `in_rate` is 0.
pub fn output_len(in_frames: usize, in_rate: u32, out_rate: u32) -> usize {
    if in_rate == 0 {
        return 0;
    }
    let num = in_frames as u128 * u128::from(out_rate);
    let den = u128::from(in_rate);
    ((num + den / 2) / den) as usize
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Resample `buffer` to `out_rate`, every channel, with exact alignment and length (see the module
/// docs). Returns a clone when the rates already match.
///
/// Errors on a sample rate outside [`MIN_SAMPLE_RATE`]–[`MAX_SAMPLE_RATE`] or ragged channels. Never
/// panics: non-finite samples are treated as silence.
pub fn resample(buffer: &AudioBuffer, out_rate: u32) -> Result<AudioBuffer, DspError> {
    let in_rate = buffer.sample_rate;
    let supported = MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE;
    if !supported.contains(&in_rate) || !supported.contains(&out_rate) {
        return Err(DspError::InvalidInput(format!(
            "cannot resample from {in_rate} Hz to {out_rate} Hz: sample rates must be between \
             {MIN_SAMPLE_RATE} and {MAX_SAMPLE_RATE} Hz"
        )));
    }
    let frames = buffer.frames();
    if buffer.channels.iter().any(|c| c.len() != frames) {
        return Err(DspError::InvalidInput(
            "channels have different lengths".into(),
        ));
    }
    let clean = buffer
        .channels
        .iter()
        .flatten()
        .all(|s| s.is_finite() && s.abs() <= MAX_SANE_MAGNITUDE);
    let sanitize = || {
        buffer
            .channels
            .iter()
            .map(|ch| ch.iter().map(|&s| sane(s)).collect::<Vec<f32>>())
            .collect::<Vec<_>>()
    };
    if in_rate == out_rate {
        return Ok(if clean {
            buffer.clone()
        } else {
            AudioBuffer::new(out_rate, sanitize())
        });
    }
    let channels = buffer.num_channels();
    let target = output_len(frames, in_rate, out_rate);
    if channels == 0 || frames == 0 {
        return Ok(AudioBuffer::new(
            out_rate,
            vec![vec![0.0; target]; channels],
        ));
    }

    // Only copy the input when it holds values the FFT cannot digest.
    let sanitized;
    let input = if clean {
        &buffer.channels
    } else {
        sanitized = sanitize();
        &sanitized
    };

    let Some(chunk) = fft_chunk(in_rate, out_rate) else {
        return Ok(direct::resample(input, in_rate, out_rate, target));
    };
    let run = || resample_fft(input, frames, in_rate, out_rate, chunk, target);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|_| {
        Err(DspError::Resample(format!(
            "{in_rate} Hz → {out_rate} Hz: the resampler failed on this audio"
        )))
    })
}

/// The input block of the synchronous FFT resampler for this ratio, or `None` when its blocks would
/// exceed [`MAX_FFT_BLOCK`] (the direct resampler is used then).
fn fft_chunk(in_rate: u32, out_rate: u32) -> Option<usize> {
    let (rate_in, rate_out) = (in_rate as usize, out_rate as usize);
    let g = gcd(rate_in, rate_out);
    let (min_block_in, min_block_out) = (rate_in / g, rate_out / g);
    // An even number of minimum blocks makes both FFT sizes even (see the module docs).
    let blocks = TARGET_BLOCK_IN
        .div_ceil(min_block_in)
        .max(1)
        .next_multiple_of(2);
    (blocks * min_block_in.max(min_block_out) <= MAX_FFT_BLOCK).then_some(blocks * min_block_in)
}

/// A sample the FFT can process: non-finite → 0, absurd magnitudes clamped.
fn sane(s: f32) -> f32 {
    if s.is_finite() {
        s.clamp(-MAX_SANE_MAGNITUDE, MAX_SANE_MAGNITUDE)
    } else {
        0.0
    }
}

/// The synchronous FFT path.
fn resample_fft(
    input: &[Vec<f32>],
    frames: usize,
    in_rate: u32,
    out_rate: u32,
    chunk: usize,
    target: usize,
) -> Result<AudioBuffer, DspError> {
    let channels = input.len();
    let mut resampler = Fft::<f32>::new_custom(
        in_rate as usize,
        out_rate as usize,
        chunk,
        1,
        channels,
        WindowFunction::BlackmanHarris2,
        FixedSync::Both,
    )
    .map_err(|e| DspError::Resample(format!("{in_rate} Hz → {out_rate} Hz: {e}")))?;

    let input = SequentialSliceOfVecs::new(input, channels, frames)
        .map_err(|e| DspError::Resample(e.to_string()))?;
    let capacity = resampler.process_all_needed_output_len(frames);
    let mut out = vec![vec![0.0f32; capacity]; channels];
    let produced = {
        let mut output = SequentialSliceOfVecs::new_mut(&mut out, channels, capacity)
            .map_err(|e| DspError::Resample(e.to_string()))?;
        let (_consumed, produced) = resampler
            .process_all_into_buffer(&input, &mut output, frames, None)
            .map_err(|e| DspError::Resample(format!("{in_rate} Hz → {out_rate} Hz: {e}")))?;
        produced
    };
    // rubato's f64 ceil can produce one extra frame; it never produces fewer than `target`, but pad
    // defensively so the length contract holds regardless.
    debug_assert!(
        produced + 1 >= target,
        "rubato produced {produced}, wanted {target}"
    );
    for ch in out.iter_mut() {
        ch.truncate(produced.min(target));
        ch.resize(target, 0.0);
    }
    Ok(AudioBuffer::new(out_rate, out))
}

/// The direct windowed-sinc resampler for ratios whose FFT blocks would be too large.
mod direct {
    use crate::AudioBuffer;

    /// Kaiser window β (≈ 100 dB stopband attenuation).
    const BETA: f64 = 10.06;
    /// Stopband attenuation the filter length is designed for, in dB.
    const ATTENUATION_DB: f64 = 100.0;
    /// The passband ends at this fraction of the lower Nyquist frequency; the stopband starts at it.
    const PASSBAND: f64 = 0.9;
    /// Table points per zero crossing of the sinc (linear interpolation error ≈ −106 dB).
    const STEPS: usize = 512;

    /// Modified Bessel function of the first kind, order 0 (power series; converges fast for the
    /// arguments a Kaiser window needs).
    fn bessel_i0(x: f64) -> f64 {
        let (mut sum, mut term, q) = (1.0, 1.0, x * x / 4.0);
        for k in 1..200 {
            term *= q / (k * k) as f64;
            sum += term;
            if term < sum * 1e-17 {
                break;
            }
        }
        sum
    }

    /// The low-pass kernel `h(x) = 2·fc·sinc(2·fc·x)·kaiser(x / W)` for a distance `x` in input samples,
    /// tabulated over `z = 2·fc·|x|` (zero crossings of the sinc).
    struct Kernel {
        /// `2·fc`: cutoff (cycles per input sample) times two.
        two_fc: f64,
        /// Half width in input samples.
        half_width: f64,
        /// `sinc(z)·kaiser(z / zeros)` at `z = i / STEPS`, plus a trailing zero.
        table: Vec<f64>,
    }

    impl Kernel {
        fn new(in_rate: u32, out_rate: u32) -> Self {
            // Cycles per input sample of the lower Nyquist frequency.
            let nyquist = 0.5 * (f64::from(out_rate) / f64::from(in_rate)).min(1.0);
            let transition = (1.0 - PASSBAND) * nyquist;
            let two_fc = (PASSBAND + 1.0) * nyquist; // cutoff at the middle of the transition band
            // Kaiser's estimate of the filter length for the attenuation and transition width.
            let taps = (ATTENUATION_DB - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * transition);
            let half_width = taps / 2.0;
            let zeros = two_fc * half_width; // ≈ 61, independent of the ratio
            let points = (zeros * STEPS as f64).ceil() as usize + 1;
            let norm = bessel_i0(BETA);
            let mut table: Vec<f64> = (0..points)
                .map(|i| {
                    let z = i as f64 / STEPS as f64;
                    let r = z / zeros;
                    if r >= 1.0 {
                        return 0.0;
                    }
                    let window = bessel_i0(BETA * (1.0 - r * r).sqrt()) / norm;
                    let sinc = if z == 0.0 {
                        1.0
                    } else {
                        (std::f64::consts::PI * z).sin() / (std::f64::consts::PI * z)
                    };
                    sinc * window
                })
                .collect();
            table.push(0.0);
            Kernel {
                two_fc,
                half_width,
                table,
            }
        }

        /// Kernel weight (without the constant `2·fc`, which normalization removes) at distance `x`.
        fn at(&self, x: f64) -> f64 {
            let pos = (self.two_fc * x.abs()) * STEPS as f64;
            let i = pos as usize;
            if i + 1 >= self.table.len() {
                return 0.0;
            }
            let f = pos - i as f64;
            self.table[i] + f * (self.table[i + 1] - self.table[i])
        }
    }

    /// Resample planar `input` from `in_rate` to `out_rate` into exactly `target` frames.
    pub(super) fn resample(
        input: &[Vec<f32>],
        in_rate: u32,
        out_rate: u32,
        target: usize,
    ) -> AudioBuffer {
        let kernel = Kernel::new(in_rate, out_rate);
        let frames = input.first().map_or(0, Vec::len) as i64;
        let reach = kernel.half_width.ceil() as i64 + 1;
        let (step_whole, step_rem, den) = (
            i64::from(in_rate / out_rate),
            u64::from(in_rate % out_rate),
            u64::from(out_rate),
        );
        let mut weights: Vec<f64> = Vec::with_capacity(2 * reach as usize + 1);
        let mut out: Vec<Vec<f32>> = vec![Vec::with_capacity(target); input.len()];
        // Output frame m sits at input position m·in/out = k + rem/out, stepped exactly in integers.
        let (mut k, mut rem) = (0i64, 0u64);
        for _ in 0..target {
            let frac = rem as f64 / den as f64;
            weights.clear();
            let mut total = 0.0f64;
            for d in -reach..=reach {
                let w = kernel.at(frac - d as f64);
                weights.push(w);
                total += w;
            }
            // Unity DC gain at every position; taps outside the input read zeros.
            let scale = if total.abs() > f64::MIN_POSITIVE {
                1.0 / total
            } else {
                0.0
            };
            let first = k - reach;
            let lo = (-first).max(0) as usize;
            let hi = ((frames - first).min(2 * reach + 1)).max(0) as usize;
            for (ch, dst) in input.iter().zip(out.iter_mut()) {
                let mut acc = 0.0f64;
                if lo < hi {
                    let start = (first + lo as i64) as usize;
                    for (w, &x) in weights[lo..hi].iter().zip(&ch[start..start + (hi - lo)]) {
                        acc += w * f64::from(x);
                    }
                }
                dst.push((acc * scale) as f32);
            }
            k += step_whole;
            rem += step_rem;
            if rem >= den {
                rem -= den;
                k += 1;
            }
        }
        AudioBuffer::new(out_rate, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(rate: u32, at_secs: f64, total_secs: f64) -> AudioBuffer {
        let n = (total_secs * f64::from(rate)).round() as usize;
        let mut v = vec![0.0f32; n];
        v[(at_secs * f64::from(rate)).round() as usize] = 1.0;
        AudioBuffer::mono(rate, v)
    }

    fn argmax_abs(x: &[f32]) -> usize {
        x.iter()
            .enumerate()
            .fold((0, 0.0f32), |(bi, bv), (i, v)| {
                if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
            })
            .0
    }

    /// Sub-sample position of the impulse peak (parabolic interpolation around the max).
    fn peak_position(x: &[f32]) -> f64 {
        let i = argmax_abs(x);
        if i == 0 || i + 1 >= x.len() {
            return i as f64;
        }
        let (a, b, c) = (f64::from(x[i - 1]), f64::from(x[i]), f64::from(x[i + 1]));
        let denom = a - 2.0 * b + c;
        if denom == 0.0 {
            i as f64
        } else {
            i as f64 + 0.5 * (a - c) / denom
        }
    }

    #[test]
    fn output_length_is_exact() {
        assert_eq!(output_len(96_000, 48_000, 44_100), 88_200);
        assert_eq!(output_len(88_200, 44_100, 48_000), 96_000);
        assert_eq!(output_len(44_100, 22_050, 48_000), 96_000);
        assert_eq!(output_len(223_081, 48_000, 44_100), 204_956); // 204955.669
        assert_eq!(output_len(1, 24_000, 48_000), 2);
        assert_eq!(output_len(3, 48_000, 32_000), 2);
        assert_eq!(output_len(0, 48_000, 44_100), 0);
        assert_eq!(output_len(10, 0, 44_100), 0);
    }

    #[test]
    fn click_stays_in_place() {
        for (from, to) in [
            (24_000, 48_000),
            (44_100, 48_000),
            (22_050, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (48_000, 24_000),
            (8000, 44_100),
        ] {
            let input = click(from, 1.0, 2.0);
            let out = resample(&input, to).unwrap();
            assert_eq!(out.sample_rate, to);
            assert_eq!(
                out.frames(),
                output_len(input.frames(), from, to),
                "{from}->{to} length"
            );
            let expected = f64::from(to);
            let at = argmax_abs(&out.channels[0]);
            assert!(
                at.abs_diff(to as usize) <= 1,
                "{from}->{to}: click at {at}, expected {expected}"
            );
            let precise = peak_position(&out.channels[0]);
            assert!(
                (precise - expected).abs() < 0.2,
                "{from}->{to}: sub-sample peak at {precise}"
            );
        }
    }

    #[test]
    fn identity_and_multichannel() {
        let buf = AudioBuffer::new(48_000, vec![vec![0.1, 0.2, 0.3], vec![-0.1, -0.2, -0.3]]);
        assert_eq!(resample(&buf, 48_000).unwrap(), buf);

        // Two channels with clicks at different times stay independent and aligned.
        let n = 48_000;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[12_000] = 1.0;
        r[36_000] = -1.0;
        let out = resample(&AudioBuffer::new(48_000, vec![l, r]), 44_100).unwrap();
        assert_eq!(out.num_channels(), 2);
        assert_eq!(out.frames(), 44_100);
        assert!(argmax_abs(&out.channels[0]).abs_diff(11_025) <= 1);
        assert!(argmax_abs(&out.channels[1]).abs_diff(33_075) <= 1);
        assert!(out.channels[1][33_075] < -0.5);
        assert!(out.channels[0][33_075].abs() < 1e-3, "no crosstalk");
    }

    #[test]
    fn sine_amplitude_is_preserved() {
        let rate = 24_000;
        let n = rate as usize;
        let tone: Vec<f32> = (0..n)
            .map(|i| {
                (0.5 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(rate)).sin())
                    as f32
            })
            .collect();
        let out = resample(&AudioBuffer::mono(rate, tone), 48_000).unwrap();
        // Ignore the edges (filter ringing against the implicit zeros).
        let mid = &out.channels[0][4_800..43_200];
        let peak = mid.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.005, "peak {peak}");
        for (k, &s) in mid.iter().enumerate().step_by(97) {
            let t = (k + 4_800) as f64 / 48_000.0;
            let ideal = 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin();
            assert!(
                (f64::from(s) - ideal).abs() < 2e-3,
                "sample {k}: {s} vs {ideal}"
            );
        }
    }

    /// `amp·sin(2π·freq·t)` for `secs` at `rate`.
    fn sine(rate: u32, secs: f64, freq: f64, amp: f64) -> Vec<f32> {
        let n = (secs * f64::from(rate)).round() as usize;
        (0..n)
            .map(|i| {
                (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin())
                    as f32
            })
            .collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn non_finite_samples_never_panic() {
        // NaN every 50th sample, +∞ every 77th (the reviewers' poisoned cache entry), at a rate that
        // needs resampling, at the output rate, and on the direct path.
        for (from, to) in [(44_100u32, 48_000u32), (48_000, 48_000), (96_001, 48_000)] {
            let n = from as usize / 2;
            let mut x = sine(from, 0.5, 330.0, 0.3);
            for i in (0..n).step_by(50) {
                x[i] = f32::NAN;
            }
            for i in (0..n).step_by(77) {
                x[i] = f32::INFINITY;
            }
            x[11] = f32::NEG_INFINITY;
            x[13] = 3e38; // finite but absurd: would overflow the FFT
            let out = resample(&AudioBuffer::new(from, vec![x.clone(), x]), to).unwrap();
            assert_eq!(out.frames(), output_len(n, from, to), "{from}->{to}");
            assert!(
                out.channels.iter().flatten().all(|s| s.is_finite()),
                "{from}->{to}: non-finite output"
            );
        }
    }

    #[test]
    fn implausible_rates_are_rejected() {
        for rate in [1u32, 999, 768_001, 4_000_037, u32::MAX] {
            let err = resample(&AudioBuffer::mono(rate, vec![0.1; 480]), 48_000).unwrap_err();
            assert!(
                err.to_string().contains("between 1000 and 768000 Hz"),
                "{err}"
            );
        }
        assert!(resample(&AudioBuffer::mono(48_000, vec![0.1]), 1_000_000).is_err());
        let out = resample(&AudioBuffer::mono(1_000, vec![0.1; 10]), 192_000).unwrap();
        assert_eq!(out.frames(), 1920);
    }

    #[test]
    fn fft_blocks_are_bounded() {
        // Common conversions keep the FFT path.
        for (from, to) in [
            (44_100, 48_000),
            (8000, 192_000),
            (192_000, 8000),
            (768_000, 44_100),
            (44_101, 48_000),
        ] {
            assert!(fft_chunk(from, to).is_some(), "{from}->{to}");
        }
        // Ratios that would need huge blocks use the direct path.
        for (from, to) in [
            (96_001, 48_000),
            (48_000, 96_001),
            (767_999, 48_000),
            (1000, 192_000),
        ] {
            assert!(fft_chunk(from, to).is_none(), "{from}->{to}");
        }
    }

    #[test]
    fn direct_path_is_exact_in_time_and_length() {
        for (from, to) in [
            (96_001, 48_000),
            (48_000, 96_001),
            (176_401, 44_100),
            (1000, 192_000),
            (102_947, 22_050),
        ] {
            assert!(fft_chunk(from, to).is_none());
            let input = click(from, 1.0, 2.0);
            let out = resample(&input, to).unwrap();
            assert_eq!(out.sample_rate, to);
            assert_eq!(
                out.frames(),
                output_len(input.frames(), from, to),
                "{from}->{to}"
            );
            let precise = peak_position(&out.channels[0]);
            assert!(
                (precise - f64::from(to)).abs() < 0.2,
                "{from}->{to}: peak at {precise}, expected {to}"
            );
        }
    }

    #[test]
    fn direct_path_preserves_the_passband_and_removes_aliases() {
        let (from, to) = (96_001u32, 48_000u32);
        // 1 kHz and 18 kHz pass at their level; DC stays exactly DC.
        for freq in [1000.0, 18_000.0] {
            let out = resample(&AudioBuffer::mono(from, sine(from, 0.5, freq, 0.5)), to).unwrap();
            let mid = &out.channels[0][2400..21_600];
            assert!(
                (peak(mid) - 0.5).abs() < 0.003,
                "{freq} Hz: peak {}",
                peak(mid)
            );
            for (k, &s) in mid.iter().enumerate().step_by(101) {
                let t = (k + 2400) as f64 / f64::from(to);
                let ideal = 0.5 * (2.0 * std::f64::consts::PI * freq * t).sin();
                assert!(
                    (f64::from(s) - ideal).abs() < 3e-3,
                    "{freq} Hz sample {k}: {s} vs {ideal}"
                );
            }
        }
        let dc = resample(&AudioBuffer::mono(from, vec![0.25; from as usize]), to).unwrap();
        assert!(
            dc.channels[0][4000..44_000]
                .iter()
                .all(|&s| (s - 0.25).abs() < 1e-6)
        );
        // 30 kHz is above the output Nyquist frequency: it must not fold back to 18 kHz.
        let alias = resample(&AudioBuffer::mono(from, sine(from, 0.5, 30_000.0, 0.5)), to).unwrap();
        let level = peak(&alias.channels[0][2400..21_600]);
        assert!(level < 1e-4, "alias at {level} (> -86 dB below the tone)");
    }

    #[test]
    fn tiny_and_empty_inputs() {
        for n in [0usize, 1, 2, 100, 1023, 1025] {
            let out = resample(&AudioBuffer::mono(24_000, vec![0.25; n]), 48_000).unwrap();
            assert_eq!(out.frames(), 2 * n);
        }
        let out = resample(&AudioBuffer::new(44_100, vec![]), 48_000).unwrap();
        assert_eq!((out.num_channels(), out.sample_rate), (0, 48_000));
        assert!(resample(&AudioBuffer::mono(0, vec![0.0]), 48_000).is_err());
        assert!(resample(&AudioBuffer::mono(48_000, vec![0.0]), 0).is_err());
        assert!(
            resample(
                &AudioBuffer {
                    sample_rate: 8000,
                    channels: vec![vec![0.0; 3], vec![0.0; 2]]
                },
                16_000
            )
            .is_err()
        );
    }
}
