//! Offline, linked true-peak limiter (DESIGN.md §4.7).
//!
//! ```text
//! ceiling  = 10^((true_peak_db − 0.1)/20)
//! peak[n]  = max over channels of the 4× oversampled magnitude around n
//! req[n]   = min(1, ceiling / peak[n])
//! h[n]     = min(req[n .. n+L))                      L = 5 ms lookahead (sliding minimum)
//! s[n]     = mean(h[n−L+1 ..= n])                    box smoothing (s ≤ req at every peak)
//! g[n]     = min(s[n], g[n−1] + (1 − g[n−1])·(1 − e^(−1/(0.080·sr))))     80 ms release
//! ```
//!
//! `g` is applied to every channel. The output is then measured with ebur128's independent 4× true-peak
//! meter; while it is above `true_peak_db` the ceiling is lowered by the excess + 0.05 dB and the input
//! is limited again (at most 4 retries).
//!
//! # Memory
//!
//! Limiting is streaming and works in place. A pass produces the gain of frame `n` from a window of
//! O(lookahead) frames after it, so every verification pass feeds its output to the meter in small
//! blocks without storing it, and only the accepted pass is written, over the input: frame `n` is
//! overwritten once no later frame's gain needs it. The measurement of that pass is the measurement of
//! the result ([`LimiterReport::stats`]), so nothing is measured twice. Memory beyond the buffer is
//! O(lookahead), whatever its length.
//!
//! # True-peak estimation
//!
//! The 4× oversampled points between frames `n` and `n+1` come from a 48-taps-per-phase polyphase
//! windowed-sinc interpolator (Blackman-Harris window, DC gain normalized per phase). Because the
//! verification meter (ebur128, which follows the BS.1770 reference design with only 12 taps per phase
//! and a Hann window) disagrees with an accurate interpolator by up to a few tenths of a dB on
//! broadband transients, each point is the larger of the two estimates: the first pass then satisfies
//! the verification instead of costing a retry that would lower the ceiling everywhere.
//! Interpolation is skipped (exactly, not approximately) where the L1 norm of the filters times the
//! local maximum of `|x|` proves that no oversampled point can exceed the ceiling.
//!
//! "Around `n`" is the support of that interpolator: an oversampled peak is a weighted sum of the 48
//! frames around it, so `peak[n]` is the largest oversampled magnitude within ±24 frames of `n`
//! (implemented as a ±24-frame widening of the sliding-minimum window). Every frame that contributes
//! to an inter-sample peak therefore gets at most the gain that peak requires, instead of only its two
//! neighbouring frames while the attack ramp leaves the farther contributing frames louder.
//!
//! # Boundaries
//!
//! Frames before the start and after the end behave as if `req = 1` there, except that the lookahead
//! window of the virtual frames before the start also sees the first frames, so the smoothing guarantee
//! `s ≤ req` holds for peaks in the first 5 ms too.

use std::collections::VecDeque;

use crate::loudness::{LoudnessStats, Meter};
use crate::{AudioBuffer, DspError};

/// Lookahead / smoothing window.
pub const LOOKAHEAD_MS: f64 = 5.0;
/// Release time constant.
pub const RELEASE_MS: f64 = 80.0;
/// Safety margin between the target and the limiter's internal ceiling.
pub const CEILING_MARGIN_DB: f64 = 0.1;
/// Extra margin added when a verification pass finds an over.
pub const RETRY_MARGIN_DB: f64 = 0.05;
/// Maximum number of re-limiting passes after a failed verification.
pub const MAX_RETRIES: u32 = 4;

/// Taps per polyphase phase (DESIGN.md requires ≥ 32).
const TAPS: usize = 48;
/// Tap `j` reads input offset `j − TAP_BEFORE` (offsets −23 ..= 24 around frame `n`).
const TAP_BEFORE: usize = TAPS / 2 - 1;
const TAP_AFTER: usize = TAPS / 2;

/// Frames per block of a streaming pass: the gains of a block are computed, then written or measured,
/// and the cancellation callback is polled once per block.
const BLOCK: usize = 4096;

/// What the limiter did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimiterReport {
    /// Largest gain reduction applied, in dB (≥ 0; 0 when the limiter never engaged).
    pub max_reduction_db: f64,
    /// Internal ceiling of the final pass in dBFS (`true_peak_db − 0.1`, lowered by retries).
    pub ceiling_db: f64,
    /// Number of limiting passes (1 + retries).
    pub passes: u32,
    /// True peak of the output measured by ebur128, in dBTP (`None` for silence).
    pub true_peak_dbtp: Option<f64>,
    /// Integrated loudness of the output from the same measurement (`None` when undefined).
    pub integrated_lufs: Option<f64>,
    /// Whether the verified true peak is at or below the requested `true_peak_db`.
    pub within_target: bool,
    /// The complete measurement of the output (the verification of the final pass), so callers need
    /// not measure it again. `true_peak_dbtp` and `integrated_lufs` repeat two of its values.
    pub stats: LoudnessStats,
}

/// Taps per phase of the BS.1770-style reference interpolator (the one ebur128 verifies with).
const REF_TAPS: usize = 12;
/// Reference tap `a` reads input offset `REF_NEWEST − a` (offsets −5 ..= 6).
const REF_NEWEST: usize = 6;

/// The 3 fractional phases (1/4, 2/4, 3/4) of the 4× interpolators.
pub(crate) struct Interpolator {
    /// The accurate estimator: 48 taps per phase, Blackman-Harris windowed sinc.
    coeffs: [[f32; TAPS]; 3],
    /// The verification meter's view: ebur128's 48-coefficient (12 per phase) Hann-windowed sinc.
    reference: [[f32; REF_TAPS]; 3],
    /// Largest L1 norm over all phases of both filters: `|y| ≤ l1 · max|x|` over the support.
    l1: f32,
}

impl Interpolator {
    pub(crate) fn new() -> Self {
        let pi = std::f64::consts::PI;
        let mut coeffs = [[0.0f32; TAPS]; 3];
        let mut l1 = 0.0f64;
        let half = TAPS as f64 / 2.0;
        for (p, phase) in coeffs.iter_mut().enumerate() {
            let frac = (p + 1) as f64 / 4.0;
            let mut taps = [0.0f64; TAPS];
            for (j, tap) in taps.iter_mut().enumerate() {
                let x = frac - (j as f64 - TAP_BEFORE as f64);
                let u = x / half; // within (-1, 1)
                let window = 0.35875
                    + 0.48829 * (pi * u).cos()
                    + 0.14128 * (2.0 * pi * u).cos()
                    + 0.01168 * (3.0 * pi * u).cos();
                let sinc = (pi * x).sin() / (pi * x); // x is never an integer
                *tap = sinc * window;
            }
            let sum: f64 = taps.iter().sum();
            let mut norm = 0.0;
            for (c, t) in phase.iter_mut().zip(taps) {
                *c = (t / sum) as f32;
                norm += (t / sum).abs();
            }
            l1 = l1.max(norm);
        }
        // ebur128 0.1.10 (`interp.rs`): coeff[j] = hann(j/48)·sinc((j − 24)/4), phase f uses
        // coeff[4a + f] on the frame `a` steps before the newest one; the newest frame is 6 frames after
        // the interpolated point's left neighbour.
        let mut reference = [[0.0f32; REF_TAPS]; 3];
        for (p, phase) in reference.iter_mut().enumerate() {
            let mut norm = 0.0;
            for (a, c) in phase.iter_mut().enumerate() {
                let j = (4 * a + p + 1) as f64;
                let w = 0.5 * (1.0 - (2.0 * pi * j / 48.0).cos());
                let m = (j - 24.0) * pi / 4.0;
                let v = w * m.sin() / m; // j − 24 is never a multiple of 4 for phases 1..=3
                *c = v as f32;
                norm += v.abs();
            }
            l1 = l1.max(norm);
        }
        Interpolator {
            coeffs,
            reference,
            l1: l1 as f32,
        }
    }

    /// Largest magnitude of the oversampled points strictly between frames `n` and `n + 1`, over all
    /// channels (zero outside the buffer), according to either interpolator.
    fn between(&self, channels: &[Vec<f32>], n: usize) -> f32 {
        let frames = channels.first().map_or(0, Vec::len);
        let mut max = 0.0f32;
        let mut window = [0.0f32; TAPS];
        for ch in channels {
            let lo = n as isize - TAP_BEFORE as isize;
            if lo >= 0 && n + TAP_AFTER < frames {
                window.copy_from_slice(&ch[lo as usize..lo as usize + TAPS]);
            } else {
                for (j, w) in window.iter_mut().enumerate() {
                    let idx = lo + j as isize;
                    *w = if idx >= 0 && (idx as usize) < frames {
                        ch[idx as usize]
                    } else {
                        0.0
                    };
                }
            }
            for phase in &self.coeffs {
                let y: f32 = phase.iter().zip(&window).map(|(c, x)| c * x).sum();
                max = max.max(y.abs());
            }
            // Reference taps a = 0..12 read offsets 6 − a, i.e. window[TAP_BEFORE + 6 − a].
            let newest = TAP_BEFORE + REF_NEWEST;
            for phase in &self.reference {
                let y: f32 = phase
                    .iter()
                    .enumerate()
                    .map(|(a, c)| c * window[newest - a])
                    .sum();
                max = max.max(y.abs());
            }
        }
        max
    }
}

/// Sliding-window maximum of `max_c |x_c|` over `[n − TAP_BEFORE, n + TAP_AFTER]`, advanced
/// monotonically in `n`. It keeps sample values, not references, so frames behind the window may be
/// overwritten.
struct LocalMax {
    deque: VecDeque<(usize, f32)>,
    next: usize,
}

impl LocalMax {
    fn new() -> Self {
        LocalMax {
            deque: VecDeque::with_capacity(TAPS + 1),
            next: 0,
        }
    }

    fn at(&mut self, channels: &[Vec<f32>], frames: usize, n: usize) -> f32 {
        let hi = (n + TAP_AFTER).min(frames.saturating_sub(1));
        while self.next <= hi && self.next < frames {
            let i = self.next;
            let v = channels.iter().fold(0.0f32, |m, ch| m.max(ch[i].abs()));
            while self.deque.back().is_some_and(|&(_, b)| b <= v) {
                self.deque.pop_back();
            }
            self.deque.push_back((i, v));
            self.next += 1;
        }
        let lo = n.saturating_sub(TAP_BEFORE);
        while self.deque.front().is_some_and(|&(i, _)| i < lo) {
            self.deque.pop_front();
        }
        self.deque.front().map_or(0.0, |&(_, v)| v)
    }
}

/// One limiting pass of `x × gain` under a linear `ceiling`, as a stream of per-frame output gains
/// `k[n] = gain · g[n]` (the output is `x[n] · k[n]` on every channel).
///
/// The sliding minimum runs over `req[n − E .. n + L + E)`: the 5 ms lookahead plus the half-width E of
/// the interpolation filter on both sides, so every frame that contributes to an oversampled peak gets
/// at most the gain that peak requires ("around n" in DESIGN.md §4.7).
///
/// Padded sequence `p[t]`: (L − 1 + E) ones, `req[0..N)`, then ones. `h_p[u] = min(p[u .. u + W))` with
/// `W = L + 2E` is h for frame `u − (L − 1)`; `s[n] = mean(h_p[n .. n + L))`.
///
/// `k[n]` is produced while `req` is computed for frame `m = n + L + E − 1`, which reads input frames
/// `m − (E − 1) ..= m + E`, i.e. nothing before `n + L`. Later outputs read later frames, so once
/// `k[n]` is out, frame `n` is never read again and may be overwritten in place.
struct Pass {
    frames: usize,
    la: usize,
    release: f32,
    gain: f32,
    abs_gain: f32,
    ceiling: f32,
    /// Inter-sample peaks are only computed where the local maximum exceeds this (see the module
    /// docs).
    threshold: f32,
    width: usize,
    front: usize,
    total: usize,
    local: LocalMax,
    /// Inter-sample peak between the previous frame and the current one.
    between_prev: f32,
    min_deque: VecDeque<(usize, f32)>,
    ring: Vec<f32>,
    ring_pos: usize,
    ring_sum: f64,
    h_count: usize,
    g_prev: f32,
    /// Smallest limiter gain `g` so far.
    g_min: f32,
    /// Position in the padded sequence.
    t: usize,
}

impl Pass {
    fn new(
        frames: usize,
        sample_rate: u32,
        gain: f32,
        ceiling: f32,
        interp: &Interpolator,
    ) -> Self {
        let sr = f64::from(sample_rate);
        let la = ((LOOKAHEAD_MS * sr / 1000.0).round() as usize).max(1);
        let release = (1.0 - (-1.0 / (RELEASE_MS / 1000.0 * sr)).exp()) as f32;
        let abs_gain = gain.abs();
        let ext = TAP_AFTER;
        let width = la + 2 * ext;
        let front = la - 1 + ext;
        Pass {
            frames,
            la,
            release,
            gain,
            abs_gain,
            ceiling,
            threshold: ceiling / (interp.l1 * abs_gain).max(f32::MIN_POSITIVE),
            width,
            front,
            total: front + frames + width,
            local: LocalMax::new(),
            between_prev: 0.0,
            min_deque: VecDeque::with_capacity(width + 1),
            ring: vec![1.0f32; la],
            ring_pos: 0,
            ring_sum: la as f64, // matches the ring's initial ones
            h_count: 0,
            g_prev: 1.0,
            g_min: 1.0,
            t: 0,
        }
    }

    /// `req[m] = min(1, ceiling / peak[m])` for input frame `m`. The inter-sample peak between `m` and
    /// `m + 1` (scaled) is computed only when it could matter.
    fn req(&mut self, channels: &[Vec<f32>], interp: &Interpolator, m: usize) -> f32 {
        let between_next = if self.local.at(channels, self.frames, m) > self.threshold {
            interp.between(channels, m) * self.abs_gain
        } else {
            0.0
        };
        let sample = channels.iter().fold(0.0f32, |acc, ch| acc.max(ch[m].abs())) * self.abs_gain;
        let peak = sample.max(self.between_prev).max(between_next);
        self.between_prev = between_next;
        if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        }
    }

    /// Replace `gains` with the output gains of the next (at most `max`) frames, in order. Empty once
    /// every frame is done.
    fn next_block(
        &mut self,
        channels: &[Vec<f32>],
        interp: &Interpolator,
        gains: &mut Vec<f32>,
        max: usize,
    ) {
        gains.clear();
        let la = self.la;
        while gains.len() < max && self.t < self.total {
            let t = self.t;
            self.t += 1;
            let value = if t >= self.front && t < self.front + self.frames {
                self.req(channels, interp, t - self.front)
            } else {
                1.0
            };
            while self.min_deque.back().is_some_and(|&(_, b)| b >= value) {
                self.min_deque.pop_back();
            }
            self.min_deque.push_back((t, value));
            if t + 1 < self.width {
                continue;
            }
            let u = t + 1 - self.width;
            while self.min_deque.front().is_some_and(|&(i, _)| i < u) {
                self.min_deque.pop_front();
            }
            let h = self.min_deque.front().map_or(1.0, |&(_, v)| v);
            self.ring_sum += f64::from(h) - f64::from(self.ring[self.ring_pos]);
            self.ring[self.ring_pos] = h;
            self.ring_pos += 1;
            if self.ring_pos == la {
                self.ring_pos = 0;
                // Re-sum exactly once per window so the running sum never drifts.
                self.ring_sum = self.ring.iter().map(|&v| f64::from(v)).sum();
            }
            self.h_count += 1;
            if self.h_count < la {
                continue;
            }
            // s for frame n = mean(h_p[n ..= n + L − 1]) = mean of the ring.
            let n = self.h_count - la;
            if n >= self.frames {
                self.t = self.total;
                break;
            }
            let s = (self.ring_sum / la as f64) as f32;
            let g = s.min(self.g_prev + (1.0 - self.g_prev) * self.release);
            self.g_prev = g;
            self.g_min = self.g_min.min(g);
            gains.push(self.gain * g);
        }
    }
}

/// Run a pass over `input × gain` under the linear `ceiling` and measure its output block by block
/// without storing it. Returns the smallest limiter gain and the measurement.
fn measure_pass(
    input: &AudioBuffer,
    gain: f32,
    ceiling: f32,
    interp: &Interpolator,
    cancel: &dyn Fn() -> bool,
) -> Result<(f32, LoudnessStats), DspError> {
    let frames = input.frames();
    let mut meter = Meter::new(input.num_channels(), input.sample_rate)?;
    let mut pass = Pass::new(frames, input.sample_rate, gain, ceiling, interp);
    let mut gains = Vec::with_capacity(BLOCK);
    let mut block: Vec<Vec<f32>> = vec![Vec::with_capacity(BLOCK); input.num_channels()];
    let mut start = 0;
    while start < frames {
        if cancel() {
            return Err(DspError::Cancelled);
        }
        pass.next_block(&input.channels, interp, &mut gains, BLOCK);
        let end = start + gains.len();
        for (out, ch) in block.iter_mut().zip(&input.channels) {
            out.clear();
            out.extend(ch[start..end].iter().zip(&gains).map(|(&x, &k)| x * k));
        }
        let planes: Vec<&[f32]> = block.iter().map(Vec::as_slice).collect();
        meter.add(&planes)?;
        start = end;
    }
    Ok((pass.g_min, meter.stats()?))
}

/// The same pass as [`measure_pass`], written over `buffer` (see [`Pass`] for why that is safe).
/// Returns the smallest limiter gain.
fn apply_pass(
    buffer: &mut AudioBuffer,
    gain: f32,
    ceiling: f32,
    interp: &Interpolator,
    cancel: &dyn Fn() -> bool,
) -> Result<f32, DspError> {
    let frames = buffer.frames();
    let mut pass = Pass::new(frames, buffer.sample_rate, gain, ceiling, interp);
    let mut gains = Vec::with_capacity(BLOCK);
    let mut start = 0;
    while start < frames {
        if cancel() {
            return Err(DspError::Cancelled);
        }
        pass.next_block(&buffer.channels, interp, &mut gains, BLOCK);
        let end = start + gains.len();
        for ch in &mut buffer.channels {
            for (x, &k) in ch[start..end].iter_mut().zip(&gains) {
                *x *= k;
            }
        }
        start = end;
    }
    Ok(pass.g_min)
}

/// What limiting `x × gain` produces, verified and measured but not yet written (see
/// [`plan_scaled`] and [`apply_plan`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LimitPlan {
    /// Linear gain applied before the limiter.
    pub(crate) gain: f32,
    /// Linear internal ceiling of the accepted pass.
    ceiling: f32,
    /// Smallest limiter gain of the accepted pass (1 when the limiter never engages).
    g_min: f32,
    /// The report, including the measurement of the output.
    pub(crate) report: LimiterReport,
}

/// Decide how to limit `input × gain` to `true_peak_db` dBTP without touching `input`: run the passes
/// (verification retries included) as measurements only. [`apply_plan`] then writes the accepted pass.
///
/// Errors on invalid buffers or settings, and with [`DspError::Cancelled`].
pub(crate) fn plan_scaled(
    input: &AudioBuffer,
    gain: f32,
    true_peak_db: f64,
    interp: &Interpolator,
    cancel: &dyn Fn() -> bool,
) -> Result<LimitPlan, DspError> {
    crate::wav::validate_buffer(input)?;
    if !true_peak_db.is_finite() || !gain.is_finite() {
        return Err(DspError::InvalidInput(format!(
            "invalid limiter settings: gain {gain}, ceiling {true_peak_db} dBTP"
        )));
    }
    let mut ceiling_db = true_peak_db - CEILING_MARGIN_DB;
    let mut passes = 0u32;
    loop {
        passes += 1;
        let ceiling = 10f64.powf(ceiling_db / 20.0) as f32;
        let (g_min, stats) = measure_pass(input, gain, ceiling, interp, cancel)?;
        let excess = stats.true_peak_dbtp.map_or(0.0, |v| v - true_peak_db);
        if excess <= 0.0 || passes > MAX_RETRIES {
            let report = LimiterReport {
                max_reduction_db: if g_min < 1.0 {
                    -20.0 * f64::from(g_min).log10()
                } else {
                    0.0
                },
                ceiling_db,
                passes,
                true_peak_dbtp: stats.true_peak_dbtp,
                integrated_lufs: stats.integrated_lufs,
                within_target: excess <= 0.0,
                stats,
            };
            return Ok(LimitPlan {
                gain,
                ceiling,
                g_min,
                report,
            });
        }
        ceiling_db -= excess + RETRY_MARGIN_DB;
    }
}

/// Write `plan` (from [`plan_scaled`] on this same, unmodified buffer) over `buffer`. The result is
/// exactly the signal the plan measured.
///
/// Errors with [`DspError::Cancelled`]; the buffer is then partly limited.
pub(crate) fn apply_plan(
    buffer: &mut AudioBuffer,
    plan: &LimitPlan,
    interp: &Interpolator,
    cancel: &dyn Fn() -> bool,
) -> Result<(), DspError> {
    if plan.g_min == 1.0 {
        // The limiter never engaged: every k[n] is exactly `gain`.
        if cancel() {
            return Err(DspError::Cancelled);
        }
        buffer.apply_gain(plan.gain);
        return Ok(());
    }
    let g_min = apply_pass(buffer, plan.gain, plan.ceiling, interp, cancel)?;
    debug_assert_eq!(
        g_min, plan.g_min,
        "the written pass differs from the measured one"
    );
    Ok(())
}

/// Limit `buffer` in place to `true_peak_db` dBTP (see the module docs).
pub fn limit(buffer: &mut AudioBuffer, true_peak_db: f64) -> Result<LimiterReport, DspError> {
    limit_in_place(buffer, 1.0, true_peak_db, &|| false)
}

/// Replace `buffer` with `buffer × gain` limited to `true_peak_db` dBTP, using no memory proportional to
/// its length (see the module docs). `cancel` is polled every few thousand frames; once it returns
/// `true` the call ends with [`DspError::Cancelled`] and the buffer is either untouched or partly
/// limited.
///
/// This is also how a master that is already limited gets a lower ceiling (e.g. to leave headroom for a
/// lossy codec): `limit_in_place(master, 1.0, lower_ceiling_db, cancel)`. To keep its loudness target as
/// well, master it again instead ([`crate::master::master_with_cancel`]).
///
/// Errors on invalid buffers (no channels, ragged channels, unsupported sample rate) or a non-finite
/// gain or target.
pub fn limit_in_place(
    buffer: &mut AudioBuffer,
    gain: f32,
    true_peak_db: f64,
    cancel: &dyn Fn() -> bool,
) -> Result<LimiterReport, DspError> {
    let interp = Interpolator::new();
    let plan = plan_scaled(buffer, gain, true_peak_db, &interp, cancel)?;
    apply_plan(buffer, &plan, &interp, cancel)?;
    Ok(plan.report)
}

/// Limit `input × gain` to `true_peak_db` dBTP and return the result (the input is untouched; prefer
/// [`limit_in_place`], which needs no second buffer).
///
/// Errors on invalid buffers (no channels, ragged channels, unsupported sample rate) or a non-finite
/// target.
pub fn limit_scaled(
    input: &AudioBuffer,
    gain: f32,
    true_peak_db: f64,
) -> Result<(AudioBuffer, LimiterReport), DspError> {
    let interp = Interpolator::new();
    let plan = plan_scaled(input, gain, true_peak_db, &interp, &|| false)?;
    let mut out = input.clone();
    apply_plan(&mut out, &plan, &interp, &|| false)?;
    Ok((out, plan.report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loudness;
    use crate::quantize::XorShift64;

    fn noise(rate: u32, secs: f64, amp: f32, seed: u64) -> Vec<f32> {
        let mut rng = XorShift64::new(seed);
        let n = (secs * f64::from(rate)) as usize;
        (0..n)
            .map(|_| amp * (rng.next_unit_f32() * 2.0 - 1.0))
            .collect()
    }

    #[test]
    fn interpolator_is_accurate() {
        let interp = Interpolator::new();
        assert!(interp.l1 > 1.0 && interp.l1 < 4.0, "l1 {}", interp.l1);
        // A 1 kHz sine at 48 kHz: interpolated points match the analytic signal closely.
        let rate = 48_000.0;
        let x: Vec<f32> = (0..2000)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate).sin() as f32)
            .collect();
        let channels = vec![x];
        for n in [500usize, 777, 1234] {
            let got = interp.between(&channels, n);
            let want = (1..4)
                .map(|k| {
                    (2.0 * std::f64::consts::PI * 1000.0 * (n as f64 + k as f64 / 4.0) / rate)
                        .sin()
                        .abs()
                })
                .fold(0.0f64, f64::max);
            assert!(
                (f64::from(got) - want).abs() < 1e-3,
                "n {n}: {got} vs {want}"
            );
        }
        // fs/4 at 45°: the samples are ±0.707, the true peak is 1.
        let y: Vec<f32> = (0..400)
            .map(|i| {
                (std::f64::consts::FRAC_PI_2 * i as f64 + std::f64::consts::FRAC_PI_4).sin() as f32
            })
            .collect();
        let tp = interp.between(&[y], 200);
        assert!((tp - 1.0).abs() < 0.01, "fs/4 true peak {tp}");
    }

    #[test]
    fn quiet_signals_pass_unchanged() {
        let x = noise(48_000, 1.0, 0.3, 7);
        let mut buf = AudioBuffer::new(48_000, vec![x.clone(), x.clone()]);
        let report = limit(&mut buf, -1.0).unwrap();
        assert_eq!(report.max_reduction_db, 0.0);
        assert_eq!(report.passes, 1);
        assert_eq!(buf.channels[0], x);
        assert!(report.within_target);
    }

    #[test]
    fn never_exceeds_the_ceiling() {
        for (rate, target) in [
            (48_000u32, -1.0f64),
            (44_100, -2.0),
            (48_000, 0.0),
            (24_000, -1.0),
        ] {
            // Loud noise bursts with isolated transients, linked stereo.
            let mut l = noise(rate, 3.0, 0.9, 11);
            let mut r = noise(rate, 3.0, 0.9, 12);
            for k in 0..20 {
                let at = (k * rate as usize) / 7;
                l[at] = 1.8;
                r[at + 3] = -2.2;
            }
            let input = AudioBuffer::new(rate, vec![l, r]);
            let (out, report) = limit_scaled(&input, 1.5, target).unwrap();
            let tp = loudness::true_peak_dbtp(&out).unwrap().unwrap();
            assert!(tp <= target + 1e-9, "{rate} Hz: true peak {tp} > {target}");
            assert!(report.within_target);
            assert!(report.max_reduction_db > 6.0);
            assert_eq!(report.true_peak_dbtp, Some(tp));
            // Linked: the per-frame gain ratio is identical on both channels.
            let n = rate as usize; // a frame where both channels are non-zero
            let g0 = out.channels[0][n] / (input.channels[0][n] * 1.5);
            let g1 = out.channels[1][n] / (input.channels[1][n] * 1.5);
            assert!((g0 - g1).abs() < 1e-5, "linked gain {g0} vs {g1}");
        }
    }

    #[test]
    fn isolated_peak_gets_exact_attack_shape_and_release() {
        let rate = 48_000;
        let n = rate as usize;
        let mut x = vec![0.1f32; n];
        let p = 24_000;
        x[p] = 1.0; // a single full-scale sample; ceiling at -1.1 dBFS needs ~1.1 dB of reduction
        let (out, report) = limit_scaled(&AudioBuffer::mono(rate, x.clone()), 1.0, -1.0).unwrap();
        let la = 240;
        let y = &out.channels[0];
        let ceiling = 10f32.powf(-1.1 / 20.0);
        assert!(
            y[p] <= ceiling * 1.0001,
            "peak {} vs ceiling {ceiling}",
            y[p]
        );
        // Before the lookahead window the signal is untouched.
        assert_eq!(y[p - 2 * la], 0.1);
        // The gain ramps down monotonically over the lookahead window...
        let gains: Vec<f32> = (p - la..=p).map(|i| y[i] / x[i]).collect();
        assert!(gains.windows(2).all(|w| w[1] <= w[0] + 1e-7));
        // ...and recovers slowly afterwards (80 ms release), never above 1.
        let g_after = |ms: usize| y[p + ms * 48] / 0.1;
        assert!(g_after(10) < g_after(50) && g_after(50) < g_after(300));
        assert!(g_after(300) <= 1.0 && g_after(300) > 0.99);
        // The deepest reduction is exactly what the peak required: 1.0 → ceiling (1.1 dB).
        assert!(
            (report.max_reduction_db - 1.1).abs() < 0.01,
            "{}",
            report.max_reduction_db
        );
        assert_eq!(report.passes, 1);
    }

    #[test]
    fn peak_in_the_first_frames_is_caught() {
        let rate = 48_000;
        let mut x = vec![0.0f32; 4800];
        x[3] = 1.0;
        x[0] = 0.9;
        let (out, report) = limit_scaled(&AudioBuffer::mono(rate, x), 1.0, -1.0).unwrap();
        assert!(report.within_target);
        let tp = loudness::true_peak_dbtp(&out).unwrap().unwrap();
        assert!(tp <= -1.0, "{tp}");
    }

    #[test]
    fn silence_and_empty() {
        let (out, report) = limit_scaled(&AudioBuffer::silent(48_000, 2, 1000), 4.0, -1.0).unwrap();
        assert_eq!(out, AudioBuffer::silent(48_000, 2, 1000));
        assert_eq!(report.true_peak_dbtp, None);
        assert!(report.within_target);
        let (out, _) = limit_scaled(&AudioBuffer::mono(48_000, vec![]), 1.0, -1.0).unwrap();
        assert!(out.is_empty());
        assert!(limit_scaled(&AudioBuffer::mono(48_000, vec![0.0]), 1.0, f64::NAN).is_err());
    }

    /// The pass as it was before it became streaming and in place: a separate output buffer filled in
    /// one loop. The streaming passes must produce exactly the same samples.
    fn reference_pass(
        input: &AudioBuffer,
        gain: f32,
        ceiling: f32,
        interp: &Interpolator,
    ) -> AudioBuffer {
        let frames = input.frames();
        let sr = f64::from(input.sample_rate);
        let la = ((LOOKAHEAD_MS * sr / 1000.0).round() as usize).max(1);
        let release = (1.0 - (-1.0 / (RELEASE_MS / 1000.0 * sr)).exp()) as f32;
        let channels = &input.channels;
        let mut out = AudioBuffer::silent(input.sample_rate, channels.len(), frames);
        let threshold = ceiling / (interp.l1 * gain.abs()).max(f32::MIN_POSITIVE);
        let mut local = LocalMax::new();
        let mut between_prev = 0.0f32;
        let mut req_at = |n: usize| -> f32 {
            let between_next = if local.at(channels, frames, n) > threshold {
                interp.between(channels, n) * gain.abs()
            } else {
                0.0
            };
            let sample = channels.iter().fold(0.0f32, |m, ch| m.max(ch[n].abs())) * gain.abs();
            let peak = sample.max(between_prev).max(between_next);
            between_prev = between_next;
            if peak > ceiling { ceiling / peak } else { 1.0 }
        };
        let width = la + 2 * TAP_AFTER;
        let front = la - 1 + TAP_AFTER;
        let mut min_deque: VecDeque<(usize, f32)> = VecDeque::new();
        let (mut ring, mut ring_pos, mut ring_sum) = (vec![1.0f32; la], 0usize, la as f64);
        let (mut h_count, mut g_prev) = (0usize, 1.0f32);
        for t in 0..front + frames + width {
            let value = if t >= front && t < front + frames {
                req_at(t - front)
            } else {
                1.0
            };
            while min_deque.back().is_some_and(|&(_, b)| b >= value) {
                min_deque.pop_back();
            }
            min_deque.push_back((t, value));
            if t + 1 < width {
                continue;
            }
            while min_deque.front().is_some_and(|&(i, _)| i < t + 1 - width) {
                min_deque.pop_front();
            }
            let h = min_deque.front().map_or(1.0, |&(_, v)| v);
            ring_sum += f64::from(h) - f64::from(ring[ring_pos]);
            ring[ring_pos] = h;
            ring_pos += 1;
            if ring_pos == la {
                ring_pos = 0;
                ring_sum = ring.iter().map(|&v| f64::from(v)).sum();
            }
            h_count += 1;
            if h_count < la {
                continue;
            }
            let n = h_count - la;
            if n >= frames {
                break;
            }
            let g = ((ring_sum / la as f64) as f32).min(g_prev + (1.0 - g_prev) * release);
            g_prev = g;
            for (o, i) in out.channels.iter_mut().zip(channels) {
                o[n] = i[n] * (gain * g);
            }
        }
        out
    }

    #[test]
    fn streaming_in_place_passes_match_the_reference() {
        let interp = Interpolator::new();
        let mut l = noise(48_000, 1.5, 0.9, 31);
        let r = noise(48_000, 1.5, 0.5, 32);
        l[1000] = 2.5;
        l[40_000] = -1.9;
        let input = AudioBuffer::new(48_000, vec![l, r]);
        let short = AudioBuffer::mono(44_100, noise(44_100, 0.004, 1.3, 33)); // shorter than the lookahead
        for (buf, gain, ceiling) in [
            (&input, 1.0f32, 0.89f32),
            (&input, 3.0, 0.5),
            (&input, 0.2, 0.89),
            (&short, 1.0, 0.7),
        ] {
            let want = reference_pass(buf, gain, ceiling, &interp);
            let (g_min, stats) = measure_pass(buf, gain, ceiling, &interp, &|| false).unwrap();
            assert_eq!(
                stats,
                loudness::measure(&want).unwrap(),
                "measured without storing"
            );
            let mut got = buf.clone();
            assert_eq!(
                apply_pass(&mut got, gain, ceiling, &interp, &|| false).unwrap(),
                g_min
            );
            assert_eq!(got, want, "gain {gain}, ceiling {ceiling}");
        }
    }

    #[test]
    fn the_report_is_the_measurement_of_the_output() {
        // Enough overs that the verification retries.
        let mut l = noise(44_100, 2.0, 0.95, 41);
        for k in 0..30 {
            l[k * 2900 + 7] = if k % 2 == 0 { 1.7 } else { -1.6 };
        }
        let input = AudioBuffer::mono(44_100, l);
        let (out, report) = limit_scaled(&input, 2.0, -0.5).unwrap();
        assert_eq!(report.stats, loudness::measure(&out).unwrap());
        assert_eq!(report.true_peak_dbtp, report.stats.true_peak_dbtp);
        assert_eq!(report.integrated_lufs, report.stats.integrated_lufs);
        let mut in_place = input.clone();
        assert_eq!(
            limit_in_place(&mut in_place, 2.0, -0.5, &|| false).unwrap(),
            report
        );
        assert_eq!(in_place, out);
        // A limiter that never engages scales exactly.
        let mut quiet = input.clone();
        let report = limit_in_place(&mut quiet, 0.25, -1.0, &|| false).unwrap();
        assert_eq!(report.max_reduction_db, 0.0);
        let mut scaled = input.clone();
        scaled.apply_gain(0.25);
        assert_eq!(quiet, scaled);
    }

    #[test]
    fn a_limited_master_gets_a_lower_ceiling() {
        let input = AudioBuffer::new(
            48_000,
            vec![noise(48_000, 2.0, 0.9, 51), noise(48_000, 2.0, 0.9, 52)],
        );
        let mut master = input.clone();
        limit_in_place(&mut master, 2.0, -1.0, &|| false).unwrap();
        let report = limit_in_place(&mut master, 1.0, -2.5, &|| false).unwrap();
        let tp = loudness::true_peak_dbtp(&master).unwrap().unwrap();
        assert!(tp <= -2.5 && report.within_target, "{tp}");
    }

    #[test]
    fn cancellation_stops_a_pass() {
        let input = AudioBuffer::mono(48_000, noise(48_000, 2.0, 1.5, 61));
        let calls = std::cell::Cell::new(0u32);
        let cancel = || {
            calls.set(calls.get() + 1);
            calls.get() > 5
        };
        let mut buf = input.clone();
        assert!(matches!(
            limit_in_place(&mut buf, 1.0, -1.0, &cancel),
            Err(DspError::Cancelled)
        ));
        assert_eq!(calls.get(), 6);
        assert_eq!(buf, input, "cancelled while measuring: untouched");
    }
}
