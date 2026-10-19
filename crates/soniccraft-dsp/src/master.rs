//! Master loudness normalization + true-peak limiting (DESIGN.md §4.6 steps 3–4).
//!
//! With `normalize`:
//!
//! 1. measure the integrated loudness `I` of the (de-clicked) mix;
//! 2. gain the mix by `G = target − I` and run the true-peak limiter at `true_peak_db`;
//! 3. re-measure: if `|I' − target| > 0.2 LU`, choose another pre-limiter gain and re-limit from the
//!    pre-limiter mix; at most 4 passes (gains tried). The result is the pass closest to the target
//!    (see below for an unreachable one).
//!
//! # Choosing the gain
//!
//! Every pass is a *hit* (within 0.2 LU, which ends the loop), *quiet* (below the target, or below the
//! −70 LUFS gate where its loudness is undefined), *loud* (above it) or *hot* (more than 6 dB of
//! average limiting, see below). The quiet pass with the largest gain and the loud or hot pass with the
//! smallest one bracket the gain to look for:
//!
//! * before a bracket exists, the gain moves by `target − I'` (DESIGN.md), divided by the slope
//!   `dI'/dG` of the last two passes (clamped to `[0.25, 1]`, since under limiting each dB of gain
//!   adds less than a dB of loudness);
//! * inside a bracket, the next gain interpolates linearly between its ends, and falls back to the
//!   midpoint when the interpolation leaves the bracket or the same end moved twice in a row. The
//!   bracket always shrinks, so the gating discontinuities of BS.1770 (a few quiet blocks crossing the
//!   relative gate can make `I'` jump by more than 1 LU between two nearby gains) cannot make the loop
//!   oscillate; when no gain lands within the tolerance, the closest pass is kept and
//!   [`MasterResult::target_reached`] is `false` (`loudness_target_missed`).
//!
//! A pass whose loudness is undefined never replaces a defined one.
//!
//! # Unreachable targets
//!
//! The target is **unreachable** when it needs more than 6 dB of average gain reduction. "Average
//! reduction" is measured the way loudness is: the loudness the limiter removed, `(I + G) − I'`. Once a
//! pass removed more than 6 dB (+0.05 dB tolerance) and is still below the target, more gain cannot
//! help: the loop looks instead for the loudest master within the limit, i.e. the gain where the
//! reduction reaches 6 dB. That search does not depend on the target: it starts from the limiter
//! onset (the gain at which the loudest true peak reaches the ceiling, where nothing is removed) plus
//! 6 dB, which can remove at most 6 dB anywhere, and follows the secant of the reduction through the
//! passes within the limit, stopping within 0.2 dB of it. A louder target therefore never yields a
//! quieter master. The result is the loudest pass within the limit, reported with `unreachable = true`
//! and the achieved loudness.
//!
//! When the loudness is undefined the mix gets no gain: `silent` (digital silence, or everything below
//! the −70 LUFS gate) or `too_short` (less than one 400 ms gating block). The limiter still runs as a
//! clip guard, exactly as without `normalize`.
//!
//! # Memory and cancellation
//!
//! Every pass is a streaming limiter run that measures its output without storing it (see
//! [`crate::limiter`]); only the chosen pass is written, over the mix. Mastering therefore needs no
//! memory proportional to the mix beyond the mix itself, and the chosen pass's measurement is the
//! report (nothing is measured twice). [`master_with_cancel`] polls a cancellation callback every few
//! thousand frames.

use crate::analysis::db_to_linear;
use crate::limiter::{self, Interpolator, LimitPlan, LimiterReport};
use crate::loudness::{self, LoudnessStats};
use crate::{AudioBuffer, DspError};

/// Loudness tolerance of the mastering loop.
pub const TARGET_TOLERANCE_LU: f64 = 0.2;
/// Maximum limiter passes (gains tried) of the loudness loop.
pub const MAX_ITERATIONS: u32 = 4;
/// Largest acceptable average limiter gain reduction (loudness removed by the limiter).
pub const MAX_AVERAGE_REDUCTION_DB: f64 = 6.0;
/// A backed-off master may exceed [`MAX_AVERAGE_REDUCTION_DB`] by this much.
pub const BACKOFF_TOLERANCE_DB: f64 = 0.05;
/// The search for the loudest master within the limit stops once a pass removes at least
/// [`MAX_AVERAGE_REDUCTION_DB`] minus this much.
pub const BACKOFF_PRECISION_DB: f64 = 0.2;
/// Integrated loudness needs at least one gating block of this length.
pub const MIN_MEASURABLE_SECS: f64 = 0.4;

/// The BS.1770 absolute gate: integrated loudness below it is undefined.
const GATE_LUFS: f64 = -70.0;
/// Largest average reduction a pass may have.
const REDUCTION_LIMIT_DB: f64 = MAX_AVERAGE_REDUCTION_DB + BACKOFF_TOLERANCE_DB;
/// A bracket narrower than this (in dB of gain) is not refined further.
const MIN_BRACKET_DB: f64 = 0.01;

/// Mastering settings (from the timeline's `output` block).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterParams {
    /// Integrated loudness target in LUFS.
    pub target_lufs: f64,
    /// True-peak ceiling in dBTP.
    pub true_peak_db: f64,
    /// Apply the loudness gain stage (`false`: limiter only, as a clip guard).
    pub normalize: bool,
}

/// What mastering did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterResult {
    /// Loudness of the input mix.
    pub before: LoudnessStats,
    /// Loudness of the mastered output (the measurement of the chosen pass, identical to measuring the
    /// output again).
    pub after: LoudnessStats,
    /// Loudness gain applied before the limiter, in dB (0 when not normalized).
    pub gain_db: f64,
    /// Deepest gain reduction of the final limiter pass, in dB.
    pub limiter_max_reduction_db: f64,
    /// Gain → limit → measure passes run (0 when nothing was done, 1 for a clip-guard-only master).
    pub iterations: u32,
    /// Whether loudness normalization ran (requested and the loudness was defined).
    pub normalized: bool,
    /// The output is within ±0.2 LU of the target (always false when not normalized).
    /// `normalized && !target_reached && !unreachable` is a missed target (`loudness_target_missed`):
    /// the output is the closest pass found, `after.integrated_lufs`.
    pub target_reached: bool,
    /// The target needs more than 6 dB of average limiting: the output is the loudest master found
    /// within that limit (`loudness_target_unreachable`, report `after.integrated_lufs` as the achieved
    /// value).
    pub unreachable: bool,
    /// The mix is silent (integrated loudness undefined although it is long enough to measure, or
    /// digital silence): no gain (`silent_output`).
    pub silent: bool,
    /// The mix is shorter than 400 ms, so its loudness cannot be measured: no gain.
    pub too_short: bool,
    /// The final limiter pass.
    pub limiter: LimiterReport,
}

/// Master `buffer` in place (see the module docs).
///
/// Errors on invalid buffers or non-finite parameters.
pub fn master(buffer: &mut AudioBuffer, params: &MasterParams) -> Result<MasterResult, DspError> {
    master_with_cancel(buffer, params, &|| false)
}

/// [`master`], polling `cancel` between passes and every few thousand frames within them: once it
/// returns `true`, mastering stops within milliseconds with [`DspError::Cancelled`], leaving `buffer`
/// unspecified (the input, or partly mastered).
///
/// Calling it again on its own output with a lower `true_peak_db` re-masters under the new ceiling and
/// the same loudness target (e.g. to leave headroom for a lossy codec): the input is then already at
/// the target, so the new `gain_db` is the small compensation for the extra limiting.
pub fn master_with_cancel(
    buffer: &mut AudioBuffer,
    params: &MasterParams,
    cancel: &dyn Fn() -> bool,
) -> Result<MasterResult, DspError> {
    if !params.target_lufs.is_finite() || !params.true_peak_db.is_finite() {
        return Err(DspError::InvalidInput(format!(
            "invalid mastering target {} LUFS / {} dBTP",
            params.target_lufs, params.true_peak_db
        )));
    }
    let before = loudness::measure_with_cancel(buffer, cancel)?;
    let digital_silence = before.sample_peak_dbfs.is_none();
    let measurable = before.integrated_lufs.is_some();
    let long_enough = buffer.duration_secs() >= MIN_MEASURABLE_SECS;
    let silent = digital_silence || (!measurable && long_enough);
    let too_short = !measurable && !long_enough && !digital_silence;
    let interp = Interpolator::new();

    let Some(i0) = before.integrated_lufs.filter(|_| params.normalize) else {
        // Clip guard only.
        let plan = limiter::plan_scaled(buffer, 1.0, params.true_peak_db, &interp, cancel)?;
        limiter::apply_plan(buffer, &plan, &interp, cancel)?;
        return Ok(MasterResult {
            before,
            after: plan.report.stats,
            gain_db: 0.0,
            limiter_max_reduction_db: plan.report.max_reduction_db,
            iterations: if digital_silence { 0 } else { 1 },
            normalized: false,
            target_reached: false,
            unreachable: false,
            silent,
            too_short,
            limiter: plan.report,
        });
    };

    let search = Search {
        i0,
        target: params.target_lufs,
        // The gain at which the loudest true peak of the mix reaches the limiter's ceiling.
        onset_db: before
            .true_peak_dbtp
            .map(|tp| params.true_peak_db - limiter::CEILING_MARGIN_DB - tp),
    };
    let input: &AudioBuffer = buffer;
    let outcome = search.run(|gain_db| {
        if cancel() {
            return Err(DspError::Cancelled);
        }
        let plan = limiter::plan_scaled(
            input,
            db_to_linear(gain_db) as f32,
            params.true_peak_db,
            &interp,
            cancel,
        )?;
        Ok(Attempt {
            gain_db,
            lufs: plan.report.integrated_lufs,
            data: plan,
        })
    })?;
    let chosen: Attempt<LimitPlan> = outcome.chosen;
    limiter::apply_plan(buffer, &chosen.data, &interp, cancel)?;
    let report = chosen.data.report;
    Ok(MasterResult {
        before,
        after: report.stats,
        gain_db: chosen.gain_db,
        limiter_max_reduction_db: report.max_reduction_db,
        iterations: outcome.iterations,
        normalized: true,
        target_reached: outcome.target_reached,
        unreachable: outcome.unreachable,
        silent: false,
        too_short: false,
        limiter: report,
    })
}

/// One pre-limiter gain tried by the loudness loop.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Attempt<T> {
    /// Gain before the limiter, in dB.
    gain_db: f64,
    /// Integrated loudness of the limited output (`None` below the −70 LUFS gate).
    lufs: Option<f64>,
    /// What produced it (the limiter plan).
    data: T,
}

/// How a pass relates to the target (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Hit,
    Quiet,
    Loud,
    Hot,
}

/// The loop's result.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Outcome<T> {
    chosen: Attempt<T>,
    iterations: u32,
    target_reached: bool,
    unreachable: bool,
}

/// The gain search of the loudness loop (see the module docs). Pure: the passes come from `evaluate`.
#[derive(Debug, Clone, Copy)]
struct Search {
    /// Integrated loudness of the mix.
    i0: f64,
    /// Target loudness.
    target: f64,
    /// Gain at which the limiter starts to act (`None` if unknown).
    onset_db: Option<f64>,
}

impl Search {
    /// Run passes until one hits the target, the budget is spent or nothing is left to refine.
    fn run<T: Copy>(
        &self,
        mut evaluate: impl FnMut(f64) -> Result<Attempt<T>, DspError>,
    ) -> Result<Outcome<T>, DspError> {
        let mut attempts: Vec<Attempt<T>> = Vec::with_capacity(MAX_ITERATIONS as usize);
        let mut gain_db = self.target - self.i0;
        loop {
            let attempt = evaluate(gain_db)?;
            attempts.push(attempt);
            if self.verdict(&attempt) == Verdict::Hit
                || attempts.len() >= MAX_ITERATIONS as usize
                || self.near_the_limit(&attempts)
            {
                break;
            }
            match self.next_gain(&attempts) {
                Some(next) => gain_db = next,
                None => break,
            }
        }
        Ok(self.finish(&attempts))
    }

    /// Loudness the limiter removed in a pass, in dB.
    fn reduction<T>(&self, a: &Attempt<T>) -> Option<f64> {
        a.lufs.map(|i| self.i0 + a.gain_db - i)
    }

    fn verdict<T>(&self, a: &Attempt<T>) -> Verdict {
        let Some(lufs) = a.lufs else {
            return Verdict::Quiet; // below the gate: needs more gain
        };
        if self.i0 + a.gain_db - lufs > REDUCTION_LIMIT_DB {
            Verdict::Hot
        } else if (lufs - self.target).abs() <= TARGET_TOLERANCE_LU {
            Verdict::Hit
        } else if lufs < self.target {
            Verdict::Quiet
        } else {
            Verdict::Loud
        }
    }

    /// Passes within the limit with a defined loudness.
    fn acceptable<'a, T>(
        &'a self,
        attempts: &'a [Attempt<T>],
    ) -> impl Iterator<Item = &'a Attempt<T>> + 'a {
        attempts
            .iter()
            .filter(|a| a.lufs.is_some() && self.verdict(a) != Verdict::Hot)
    }

    /// A pass was over-limited and still too quiet, and none within the limit is too loud: the target
    /// is out of reach.
    fn out_of_reach<T>(&self, attempts: &[Attempt<T>]) -> bool {
        let over = attempts.iter().any(|a| {
            self.verdict(a) == Verdict::Hot
                && a.lufs
                    .is_some_and(|i| i < self.target - TARGET_TOLERANCE_LU)
        });
        over && !self
            .acceptable(attempts)
            .any(|a| matches!(self.verdict(a), Verdict::Loud | Verdict::Hit))
    }

    /// Out of reach, and the loudest pass within the limit is within the precision of it.
    fn near_the_limit<T>(&self, attempts: &[Attempt<T>]) -> bool {
        self.out_of_reach(attempts)
            && self.acceptable(attempts).any(|a| {
                self.reduction(a)
                    .is_some_and(|r| r >= MAX_AVERAGE_REDUCTION_DB - BACKOFF_PRECISION_DB)
            })
    }

    /// The gain to try next, or `None` when nothing is left to refine.
    fn next_gain<T>(&self, attempts: &[Attempt<T>]) -> Option<f64> {
        let lo = attempts
            .iter()
            .filter(|a| self.verdict(a) == Verdict::Quiet)
            .max_by(|a, b| a.gain_db.total_cmp(&b.gain_db));
        let hi = attempts
            .iter()
            .filter(|a| matches!(self.verdict(a), Verdict::Loud | Verdict::Hot))
            .min_by(|a, b| a.gain_db.total_cmp(&b.gain_db));
        let next = match (lo, hi) {
            (Some(lo), Some(hi)) if lo.gain_db < hi.gain_db => self.inside(lo, hi, attempts)?,
            // Inconsistent passes (a louder one at a lower gain): refine below the loud one.
            (Some(_), Some(hi)) | (None, Some(hi)) => self.downward(hi, attempts)?,
            (Some(lo), None) => self.upward(lo, attempts)?,
            (None, None) => return None,
        };
        let fresh = attempts.iter().all(|a| (a.gain_db - next).abs() > 1e-6);
        (next.is_finite() && fresh).then_some(next)
    }

    /// Every pass so far is too quiet: step up from the loudest one.
    fn upward<T>(&self, lo: &Attempt<T>, attempts: &[Attempt<T>]) -> Option<f64> {
        let Some(lufs) = lo.lufs else {
            // Below the gate there is no loudness to step from: +1 dB, doubling with every such pass.
            let undefined = attempts.iter().filter(|a| a.lufs.is_none()).count();
            return Some(lo.gain_db + f64::powi(2.0, undefined.saturating_sub(1) as i32));
        };
        let mut quiet: Vec<(f64, f64)> = attempts
            .iter()
            .filter_map(|a| a.lufs.map(|i| (a.gain_db, i)))
            .filter(|&(g, _)| g <= lo.gain_db)
            .collect();
        quiet.sort_by(|a, b| b.0.total_cmp(&a.0));
        let slope = secant_slope(&quiet);
        Some(lo.gain_db + (self.target - lufs) / slope)
    }

    /// Every pass so far is too loud or over-limited: step down from the one with the lowest gain.
    fn downward<T>(&self, hi: &Attempt<T>, attempts: &[Attempt<T>]) -> Option<f64> {
        let lufs = hi.lufs?;
        if self.verdict(hi) == Verdict::Loud {
            let mut loud: Vec<(f64, f64)> = attempts
                .iter()
                .filter(|a| self.verdict(a) == Verdict::Loud)
                .filter_map(|a| a.lufs.map(|i| (a.gain_db, i)))
                .filter(|&(g, _)| g >= hi.gain_db)
                .collect();
            loud.sort_by(|a, b| a.0.total_cmp(&b.0));
            return Some(hi.gain_db - (lufs - self.target) / secant_slope(&loud));
        }
        // Over-limited. The reduction grows by at most 1 dB per dB of gain, so the limit is at least
        // `r − MAX` dB lower (and the target at least `lufs − target` dB lower).
        let r = self.reduction(hi)?;
        let cap = hi.gain_db - (r - MAX_AVERAGE_REDUCTION_DB).max(lufs - self.target);
        if !self.out_of_reach(attempts) {
            return Some(cap.min(hi.gain_db - 0.1));
        }
        // Out of reach and nothing within the limit yet. At `onset + MAX` the deepest gain reduction,
        // and therefore the average one, is about MAX: try it, then half of it, then the onset itself
        // (no limiting at all). These do not depend on the target.
        let ladder = self.onset_db.map(|onset| {
            [
                onset + MAX_AVERAGE_REDUCTION_DB,
                onset + MAX_AVERAGE_REDUCTION_DB / 2.0,
                onset,
            ]
        });
        let tried = |g: f64| attempts.iter().any(|a| (a.gain_db - g).abs() <= 1e-6);
        let step = ladder
            .into_iter()
            .flatten()
            .find(|&g| g < hi.gain_db - MIN_BRACKET_DB && !tried(g));
        Some(step.map_or(cap, |g| g.min(cap)))
    }

    /// A gain strictly inside the bracket `(lo, hi)`.
    fn inside<T>(&self, lo: &Attempt<T>, hi: &Attempt<T>, attempts: &[Attempt<T>]) -> Option<f64> {
        let width = hi.gain_db - lo.gain_db;
        if width < MIN_BRACKET_DB {
            return None;
        }
        let midpoint = lo.gain_db + width / 2.0;
        // Keep a little distance from the ends, so every pass shrinks the bracket.
        let margin = (width * 0.02).clamp(MIN_BRACKET_DB / 2.0, 2.0 * MIN_BRACKET_DB);
        let within = |g: f64| g.is_finite() && g >= lo.gain_db + margin && g <= hi.gain_db - margin;
        if self.out_of_reach(attempts) {
            // The loudest master within the limit: where the reduction reaches MAX.
            return Some(
                self.limit_crossing(lo, hi, attempts)
                    .filter(|&g| within(g))
                    .unwrap_or(midpoint),
            );
        }
        // The same end moved twice in a row: the interpolation is stuck on one side (e.g. across a
        // gating jump), bisect.
        if let [.., a, b] = attempts {
            let side = |x: &Attempt<T>| self.verdict(x) == Verdict::Quiet;
            if attempts.len() >= 3 && side(a) == side(b) {
                return Some(midpoint);
            }
        }
        // Where I' crosses the target, by linear interpolation (below the gate counts as −70 LUFS).
        let i_lo = lo.lufs.unwrap_or(GATE_LUFS);
        let to_target = hi
            .lufs
            .filter(|&i_hi| i_hi > i_lo)
            .map(|i_hi| lo.gain_db + (self.target - i_lo) * width / (i_hi - i_lo));
        let estimate = if self.verdict(hi) == Verdict::Hot {
            // The target may lie beyond the limit: do not go past where the reduction reaches MAX.
            match (to_target, self.limit_crossing(lo, hi, attempts)) {
                (Some(t), Some(l)) => Some(t.min(l)),
                (t, l) => t.or(l),
            }
        } else {
            to_target
        };
        Some(estimate.filter(|&g| within(g)).unwrap_or(midpoint))
    }

    /// Estimate of the gain where the average reduction reaches [`MAX_AVERAGE_REDUCTION_DB`], between
    /// `lo` (within the limit) and `hi` (over it). The secant through the two loudest passes within the
    /// limit, or through the limiter onset (where nothing is removed) and the loudest one, does not
    /// depend on the target; interpolating to `hi` is the fallback when the secant leaves the bracket.
    fn limit_crossing<T>(
        &self,
        lo: &Attempt<T>,
        hi: &Attempt<T>,
        attempts: &[Attempt<T>],
    ) -> Option<f64> {
        let r_lo = self.reduction(lo)?;
        let mut within: Vec<(f64, f64)> = self
            .acceptable(attempts)
            .filter(|a| a.gain_db < lo.gain_db)
            .filter_map(|a| self.reduction(a).map(|r| (a.gain_db, r)))
            .collect();
        within.sort_by(|a, b| b.0.total_cmp(&a.0));
        let below = within.first().copied().or_else(|| {
            self.onset_db
                .filter(|&onset| onset < lo.gain_db - MIN_BRACKET_DB)
                .map(|onset| (onset, 0.0))
        });
        let secant = below.and_then(|(g, r)| {
            let slope = (r_lo - r) / (lo.gain_db - g);
            (slope > 0.0)
                .then(|| lo.gain_db + (MAX_AVERAGE_REDUCTION_DB - r_lo) / slope.clamp(0.05, 1.0))
        });
        let interpolated = self.reduction(hi).and_then(|r_hi| {
            (r_hi > r_lo).then(|| {
                lo.gain_db
                    + (MAX_AVERAGE_REDUCTION_DB - r_lo) * (hi.gain_db - lo.gain_db) / (r_hi - r_lo)
            })
        });
        secant
            .filter(|&g| g > lo.gain_db && g < hi.gain_db)
            .or(interpolated)
    }

    /// The pass to keep: a hit (the closest), else the defined pass within the limit closest to the
    /// target, else the least over-limited pass, else (everything below the gate) the loudest gain.
    fn finish<T: Copy>(&self, attempts: &[Attempt<T>]) -> Outcome<T> {
        let iterations = attempts.len() as u32;
        let error = |a: &Attempt<T>| a.lufs.map_or(f64::INFINITY, |i| (i - self.target).abs());
        let outcome = |chosen: &Attempt<T>, target_reached, unreachable| Outcome {
            chosen: *chosen,
            iterations,
            target_reached,
            unreachable,
        };
        if let Some(closest) = self
            .acceptable(attempts)
            .min_by(|a, b| error(a).total_cmp(&error(b)))
        {
            return match self.verdict(closest) {
                Verdict::Hit => outcome(closest, true, false),
                // Below the target with a louder pass over the limit: the limit stopped it.
                Verdict::Quiet if attempts.iter().any(|a| self.verdict(a) == Verdict::Hot) => {
                    outcome(closest, false, true)
                }
                _ => outcome(closest, false, false),
            };
        }
        let least_hot = attempts
            .iter()
            .filter(|a| self.verdict(a) == Verdict::Hot)
            .min_by(|a, b| {
                let r = |x: &Attempt<T>| self.reduction(x).unwrap_or(f64::INFINITY);
                r(a).total_cmp(&r(b))
            });
        match least_hot {
            Some(hot) => outcome(hot, false, true),
            None => {
                let loudest = attempts
                    .iter()
                    .max_by(|a, b| a.gain_db.total_cmp(&b.gain_db))
                    .expect("the loop runs at least one pass");
                outcome(loudest, false, false)
            }
        }
    }
}

/// Slope of the loudness over the gain from the first two `(gain, lufs)` points (the most relevant
/// first), clamped to `[0.25, 1]`; 1 with fewer than two points (the plain DESIGN update `target − I'`).
fn secant_slope(points: &[(f64, f64)]) -> f64 {
    match points {
        [(g_a, i_a), (g_b, i_b), ..] if (g_a - g_b).abs() > 1e-9 => {
            let slope = (i_a - i_b) / (g_a - g_b);
            if slope.is_finite() {
                slope.clamp(0.25, 1.0)
            } else {
                1.0
            }
        }
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantize::XorShift64;

    /// Music-like material: noise bursts with a slow envelope plus sharp transients, stereo.
    fn music_like(rate: u32, secs: f64, level: f32, seed: u64) -> AudioBuffer {
        let mut rng = XorShift64::new(seed);
        let n = (secs * f64::from(rate)) as usize;
        let mut l = Vec::with_capacity(n);
        let mut r = Vec::with_capacity(n);
        let mut lp_l = 0.0f32;
        let mut lp_r = 0.0f32;
        for i in 0..n {
            let t = i as f32 / rate as f32;
            let burst = if (t * 2.0).fract() < 0.6 { 1.0 } else { 0.15 };
            let env = level * burst * (0.6 + 0.4 * (t * 0.9).sin());
            lp_l += 0.25 * ((rng.next_unit_f32() * 2.0 - 1.0) - lp_l);
            lp_r += 0.25 * ((rng.next_unit_f32() * 2.0 - 1.0) - lp_r);
            let mut a = lp_l * env * 3.0;
            let mut b = lp_r * env * 3.0;
            if i % (rate as usize / 3) == 0 {
                a += 0.9 * level * 4.0;
                b -= 0.8 * level * 4.0;
            }
            l.push(a);
            r.push(b);
        }
        AudioBuffer::new(rate, vec![l, r])
    }

    /// The reviewers' impulsive material: uniform noise of ±0.01 with a 0.9 click every 4800 frames,
    /// stereo 48 kHz (about −31 LUFS).
    fn clicks(secs: f64, seed: u64) -> AudioBuffer {
        let mut rng = XorShift64::new(seed);
        let n = (secs * 48_000.0) as usize;
        let mut channels = vec![Vec::with_capacity(n), Vec::with_capacity(n)];
        for i in 0..n {
            for ch in &mut channels {
                let noise = 0.01 * (rng.next_unit_f32() * 2.0 - 1.0);
                ch.push(if i % 4800 == 0 { 0.9 } else { noise });
            }
        }
        AudioBuffer::new(48_000, channels)
    }

    fn params(target_lufs: f64, true_peak_db: f64) -> MasterParams {
        MasterParams {
            target_lufs,
            true_peak_db,
            normalize: true,
        }
    }

    /// Loudness the limiter removed.
    fn removed(res: &MasterResult) -> f64 {
        res.before.integrated_lufs.unwrap() + res.gain_db - res.after.integrated_lufs.unwrap()
    }

    #[test]
    fn hits_the_target_with_true_peak_below_ceiling() {
        for (target, ceiling, level) in [
            (-16.0, -1.0, 0.05f32),
            (-14.0, -1.0, 0.3),
            (-23.0, -1.0, 0.2),
            (-24.0, -2.0, 0.02),
        ] {
            let mut buf = music_like(48_000, 12.0, level, 99);
            let res = master(&mut buf, &params(target, ceiling)).unwrap();
            let i = res.after.integrated_lufs.unwrap();
            assert!(
                (i - target).abs() <= 0.2,
                "target {target}: got {i} ({res:?})"
            );
            assert!(
                res.after.true_peak_dbtp.unwrap() <= ceiling,
                "true peak {:?}",
                res.after.true_peak_dbtp
            );
            assert!(res.target_reached && res.normalized && !res.unreachable && !res.silent);
            assert!(res.iterations >= 1 && res.iterations <= MAX_ITERATIONS);
            let expected_gain = target - res.before.integrated_lufs.unwrap();
            assert!(
                res.gain_db >= expected_gain - 0.01,
                "gain {} vs naive {expected_gain}",
                res.gain_db
            );
            // The report is the measurement of the output, without measuring it again.
            assert_eq!(res.after, loudness::measure(&buf).unwrap());
        }
    }

    #[test]
    fn unreachable_target_backs_off() {
        // Very peaky material pushed to -6 LUFS with a -1 dBTP ceiling.
        let mut buf = music_like(48_000, 8.0, 0.05, 5);
        let res = master(&mut buf, &params(-5.0, -1.0)).unwrap();
        assert!(res.unreachable, "{res:?}");
        assert!(!res.target_reached);
        let achieved = res.after.integrated_lufs.unwrap();
        assert!(achieved < -5.5, "achieved {achieved}");
        assert!(
            removed(&res) <= MAX_AVERAGE_REDUCTION_DB + 0.05,
            "limiter removed {} dB",
            removed(&res)
        );
        assert!(res.after.true_peak_dbtp.unwrap() <= -1.0);
        assert_eq!(res.after, loudness::measure(&buf).unwrap());
    }

    /// review-dsp/master-backoff-overshoot: raising an unreachable target made the master quieter
    /// (target -5 gave -46.4 LUFS from a -30.8 LUFS mix, target -23 gave -30.2).
    #[test]
    fn louder_targets_never_give_quieter_masters() {
        let mut achieved = Vec::new();
        for target in [-23.0, -16.0, -10.0, -9.0, -8.0, -7.0, -6.0, -5.0] {
            let mut buf = clicks(6.0, 77);
            let res = master(&mut buf, &params(target, -1.0)).unwrap();
            let i = res.after.integrated_lufs.unwrap();
            let before = res.before.integrated_lufs.unwrap();
            assert!(res.unreachable && !res.target_reached, "{target}: {res:?}");
            // The loudest master within the limit: close to 6 dB of limiting, never more.
            let r = removed(&res);
            eprintln!(
                "target {target}: {before:.2} → {i:.3} LUFS, gain {:+.2} dB, removed {r:.3} dB, {} passes",
                res.gain_db, res.iterations
            );
            assert!(
                (MAX_AVERAGE_REDUCTION_DB - BACKOFF_PRECISION_DB - 0.05
                    ..=MAX_AVERAGE_REDUCTION_DB + BACKOFF_TOLERANCE_DB)
                    .contains(&r),
                "target {target}: removed {r} dB"
            );
            assert!(
                i > before,
                "target {target}: {i} is quieter than the mix ({before})"
            );
            assert!(res.after.true_peak_dbtp.unwrap() <= -1.0);
            if let Some(&(previous, quieter)) = achieved.last() {
                assert!(
                    i >= quieter,
                    "target {target} gives {i} LUFS, quieter than {quieter} for target {previous}"
                );
            }
            achieved.push((target, i));
        }
        // Beyond reach, the result does not depend on the target at all.
        let deep: Vec<f64> = achieved.iter().skip(2).map(|&(_, i)| i).collect();
        assert!(deep.windows(2).all(|w| w[0] == w[1]), "{achieved:?}");
    }

    #[test]
    fn silent_and_short_inputs_get_no_gain() {
        let mut silent = AudioBuffer::silent(48_000, 2, 96_000);
        let res = master(&mut silent, &params(-16.0, -1.0)).unwrap();
        assert!(res.silent && !res.normalized && !res.too_short);
        assert_eq!(res.gain_db, 0.0);
        assert_eq!(silent, AudioBuffer::silent(48_000, 2, 96_000));

        // 200 ms of loud audio: unmeasurable, but still clip-guarded.
        let mut short = AudioBuffer::mono(
            48_000,
            (0..9600)
                .map(|i| if i % 100 == 0 { 1.2 } else { 0.1 })
                .collect(),
        );
        let res = master(&mut short, &params(-16.0, -1.0)).unwrap();
        assert!(res.too_short && !res.silent && !res.normalized);
        assert_eq!(res.gain_db, 0.0);
        assert!(res.after.true_peak_dbtp.unwrap() <= -1.0);
        assert_eq!(res.after, loudness::measure(&short).unwrap());
    }

    #[test]
    fn normalize_false_is_a_clip_guard() {
        let mut buf = music_like(44_100, 6.0, 0.5, 3);
        let before = loudness::measure(&buf).unwrap();
        let res = master(
            &mut buf,
            &MasterParams {
                normalize: false,
                ..params(-16.0, -1.0)
            },
        )
        .unwrap();
        assert!(!res.normalized && !res.target_reached);
        assert_eq!(res.gain_db, 0.0);
        assert_eq!(res.iterations, 1);
        assert!(res.after.true_peak_dbtp.unwrap() <= -1.0);
        assert!(
            before.true_peak_dbtp.unwrap() > -1.0,
            "the test signal must need limiting"
        );
        assert!(res.limiter_max_reduction_db > 0.0);
        assert_eq!(res.after, loudness::measure(&buf).unwrap());
        assert!(master(&mut buf, &params(f64::NAN, -1.0)).is_err());
    }

    /// review-dsp/target-minus70-null: a pass below the -70 LUFS gate never replaces a defined one.
    #[test]
    fn targets_at_the_gate_keep_a_defined_loudness() {
        for target in [-70.0, -69.9, -60.0] {
            let mut buf = music_like(48_000, 6.0, 0.2, 8);
            let res = master(&mut buf, &params(target, -1.0)).unwrap();
            let i = res.after.integrated_lufs;
            assert!(i.is_some(), "target {target}: {res:?}");
            assert!((i.unwrap() - target).abs() < 1.0, "target {target}: {i:?}");
            assert!(res.normalized && !res.silent && !res.unreachable);
            assert_eq!(res.after, loudness::measure(&buf).unwrap());
        }
    }

    #[test]
    fn re_mastering_under_a_lower_ceiling_keeps_the_target() {
        let mut buf = music_like(48_000, 8.0, 0.3, 21);
        let first = master(&mut buf, &params(-14.0, -1.0)).unwrap();
        assert!(first.target_reached);
        let second = master(&mut buf, &params(-14.0, -2.0)).unwrap();
        assert!(second.target_reached, "{second:?}");
        assert!(second.after.true_peak_dbtp.unwrap() <= -2.0);
        assert!(
            second.gain_db.abs() < 1.0,
            "a small compensation: {}",
            second.gain_db
        );
    }

    #[test]
    fn cancellation_stops_mastering() {
        let calls = std::cell::Cell::new(0u32);
        for cancel_after in [0u32, 3, 40, 200] {
            calls.set(0);
            let mut buf = music_like(48_000, 30.0, 0.05, 4);
            let cancel = || {
                calls.set(calls.get() + 1);
                calls.get() > cancel_after
            };
            let err = master_with_cancel(&mut buf, &params(-16.0, -1.0), &cancel).unwrap_err();
            assert!(matches!(err, DspError::Cancelled), "{err}");
            // Polled once more after the request at most: it stopped right away.
            assert_eq!(calls.get(), cancel_after + 1);
        }
        let never = || false;
        let mut buf = music_like(48_000, 2.0, 0.05, 4);
        assert!(master_with_cancel(&mut buf, &params(-16.0, -1.0), &never).is_ok());
    }

    // ---- the gain search on synthetic loudness curves ------------------------------------------------

    /// Run the search on `lufs(gain)`.
    fn search(
        i0: f64,
        target: f64,
        onset_db: Option<f64>,
        lufs: impl Fn(f64) -> Option<f64>,
    ) -> (Outcome<()>, Vec<f64>) {
        let tried = std::cell::RefCell::new(Vec::new());
        let outcome = Search {
            i0,
            target,
            onset_db,
        }
        .run(|gain_db| {
            tried.borrow_mut().push(gain_db);
            Ok(Attempt {
                gain_db,
                lufs: lufs(gain_db),
                data: (),
            })
        })
        .unwrap();
        (outcome, tried.into_inner())
    }

    /// A limiter that removes `15·(1 − e^(−x/15))` dB at `x` dB above its onset (at most 1 dB per dB),
    /// like impulsive material over a steady floor: 6 dB at 7.66 dB above the onset.
    fn saturating(i0: f64, onset: f64) -> impl Fn(f64) -> Option<f64> {
        move |g: f64| {
            let x = (g - onset).max(0.0);
            Some(i0 + g - 15.0 * (1.0 - (-x / 15.0).exp()))
        }
    }

    #[test]
    fn search_hits_a_linear_response_in_one_or_two_passes() {
        let (out, tried) = search(-30.0, -16.0, Some(29.0), |g| Some(-30.0 + g));
        assert!(out.target_reached && tried == vec![14.0]);
        // Mild limiting: the second pass corrects by `target − I'`.
        let (out, tried) = search(-30.0, -16.0, Some(10.0), |g| Some(-30.0 + g - 0.04 * g));
        assert!(out.target_reached, "{out:?} {tried:?}");
        assert_eq!(tried.len(), 2);
    }

    /// review-dsp/master-nonconvergence-silent: I'(G) jumps 1.65 LU at G = 5.38 (quiet blocks crossing
    /// the relative gate); the old secant oscillated and returned the last pass, 1.6 LU off.
    #[test]
    fn a_gating_jump_keeps_the_closest_pass() {
        let i0 = -22.4;
        let jump = |g: f64| {
            Some(if g < 5.38 {
                i0 + g - 0.15
            } else {
                i0 + g + 1.49
            })
        };
        let (out, tried) = search(i0, -16.0, Some(3.0), jump);
        assert!(!out.target_reached && !out.unreachable, "{out:?}");
        assert_eq!(out.iterations, MAX_ITERATIONS);
        let error = |g: f64| (jump(g).unwrap() + 16.0).abs();
        let best = tried
            .iter()
            .map(|&g| error(g))
            .fold(f64::INFINITY, f64::min);
        assert_eq!(error(out.chosen.gain_db), best, "the closest pass is kept");
        assert!(best < 0.7, "{tried:?}");
    }

    #[test]
    fn out_of_reach_targets_converge_to_the_limit_independently_of_the_target() {
        let (i0, onset) = (-30.8, -0.18);
        let mut results = Vec::new();
        for target in [-23.0, -12.0, -10.0, -7.5, -5.0] {
            let (out, tried) = search(i0, target, Some(onset), saturating(i0, onset));
            assert!(out.unreachable && !out.target_reached, "{target}: {out:?}");
            let r = i0 + out.chosen.gain_db - out.chosen.lufs.unwrap();
            assert!(
                (MAX_AVERAGE_REDUCTION_DB - BACKOFF_PRECISION_DB..=REDUCTION_LIMIT_DB).contains(&r),
                "{target}: {r} dB removed after {tried:?}"
            );
            results.push(out.chosen.lufs.unwrap());
        }
        assert!(results.windows(2).all(|w| w[0] == w[1]), "{results:?}");
        // The boundary is at 7.66 dB above the onset: -30.8 + 7.48 - 6 = -29.32 LUFS.
        assert!((results[0] + 29.32).abs() < 0.1, "{results:?}");
        // Just below the limit, the target is reached.
        let (out, _) = search(i0, -29.6, Some(onset), saturating(i0, onset));
        assert!(out.target_reached, "{out:?}");
    }

    #[test]
    fn passes_below_the_gate_never_win() {
        // No loudness below -69.95 LUFS (the gate), none at all for gains under -49.9.
        let gate = |g: f64| Some(-20.0 + g).filter(|&i| i >= -69.95);
        let (out, tried) = search(-20.0, -70.0, Some(19.0), gate);
        assert!(out.chosen.lufs.is_some(), "{out:?} after {tried:?}");
        assert!(!out.unreachable);
        // Everything undefined: the highest gain is reported, without a loudness.
        let (out, tried) = search(-20.0, -70.0, Some(19.0), |_| None);
        assert_eq!(out.chosen.lufs, None);
        assert_eq!(tried.len(), MAX_ITERATIONS as usize);
        assert!(tried.windows(2).all(|w| w[1] > w[0]), "{tried:?}");
    }

    #[test]
    fn slopes_are_clamped() {
        assert_eq!(secant_slope(&[(10.0, -18.0)]), 1.0);
        assert_eq!(secant_slope(&[(12.0, -17.0), (10.0, -18.0)]), 0.5);
        assert_eq!(secant_slope(&[(12.0, -18.0), (10.0, -18.0)]), 0.25);
        assert_eq!(secant_slope(&[(12.0, -10.0), (10.0, -18.0)]), 1.0);
        assert_eq!(secant_slope(&[]), 1.0);
    }
}
