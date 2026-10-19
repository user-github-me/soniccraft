//! Ducking (DESIGN.md §4.5): detect trigger activity on 1 ms blocks, apply hold and lookahead, and turn
//! the result into a gain envelope that ramps linearly in dB with exact attack/release slopes.
//!
//! ```text
//! detector (sidechain RMS or cue intervals) → Activity → .hold() → .lookahead() → DuckEnvelope
//!                                                         regions() ┘              ├ apply(stem)
//!                                                                                  └ sample_gain_db(pps)
//! ```
//!
//! Block `b` covers frames `[⌊b·sr/1000⌋, ⌊(b+1)·sr/1000⌋)` (exact integer boundaries, so 44.1 kHz blocks
//! alternate between 44 and 45 frames). Durations in milliseconds are rounded to whole blocks.
//!
//! [`Activity`] keeps one flag per block, which a render needs anyway (its stems are that long).
//! [`cue_regions`] computes the cue-mode regions of dry runs from the intervals alone, with the same
//! result and a cost that does not grow with the timeline's length.

use std::ops::Range;

use crate::{AudioBuffer, DspError};

/// Length of the sliding RMS window of the sidechain detector.
pub const DETECTOR_WINDOW_MS: f64 = 10.0;

/// Ramps closer than this to their target snap onto it (absorbs f64 accumulation error).
const RAMP_SNAP_DB: f64 = 1e-9;

/// Ducking parameters (DESIGN.md §2.12). Times are in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckParams {
    /// Depth; the sign is ignored (`-18` and `18` both attenuate by 18 dB).
    pub reduction_db: f64,
    /// Time to ramp from 0 dB to full depth.
    pub attack_ms: f64,
    /// Time to ramp from full depth back to 0 dB.
    pub release_ms: f64,
    /// Gaps shorter than this are bridged, and activity is held this long after it stops.
    pub hold_ms: f64,
    /// Activity is detected this much earlier.
    pub lookahead_ms: f64,
    /// Sidechain threshold in dBFS (RMS).
    pub threshold_db: f64,
}

impl Default for DuckParams {
    /// The DSL defaults: −15 dB, 80 ms attack, 400 ms release, 250 ms hold, lookahead = attack,
    /// −40 dBFS threshold.
    fn default() -> Self {
        DuckParams {
            reduction_db: 15.0,
            attack_ms: 80.0,
            release_ms: 400.0,
            hold_ms: 250.0,
            lookahead_ms: 80.0,
            threshold_db: -40.0,
        }
    }
}

/// Rounds a non-negative millisecond duration to whole 1 ms blocks (negative / NaN → 0).
fn ms_to_blocks(ms: f64) -> usize {
    if ms.is_finite() && ms > 0.0 {
        ms.round() as usize
    } else {
        0
    }
}

/// Number of 1 ms blocks covering `frames` frames.
fn block_count(sample_rate: u32, frames: usize) -> usize {
    if sample_rate == 0 {
        return 0;
    }
    (frames as u128 * 1000).div_ceil(u128::from(sample_rate)) as usize
}

/// First frame of block `b` (may exceed the timeline for the block after the last).
fn block_start(sample_rate: u32, b: usize) -> usize {
    (b as u128 * u128::from(sample_rate) / 1000) as usize
}

/// The block containing frame `f` (`sample_rate > 0`).
fn block_of(sample_rate: u32, f: usize) -> usize {
    // start(b) <= f  <=>  b <= ceil((f + 1)·1000 / sr) − 1
    (((f as u128 + 1) * 1000 - 1) / u128::from(sample_rate)) as usize
}

/// Sort block runs and merge the ones that overlap or touch: the maximal runs of their union.
fn union(mut runs: Vec<Range<usize>>) -> Vec<Range<usize>> {
    runs.retain(|r| r.start < r.end);
    runs.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(runs.len());
    for run in runs {
        match out.last_mut() {
            Some(last) if run.start <= last.end => last.end = last.end.max(run.end),
            _ => out.push(run),
        }
    }
    out
}

/// The active regions of cue-mode ducking: exactly what
/// `DuckEnvelope::cue(intervals, sample_rate, frames, params).regions()` returns (DESIGN.md §4.5 steps
/// 1–3), computed on the block runs of the intervals instead of one flag per block. Time is
/// O(k log k) and memory O(k) in the number of intervals, independent of `frames`, so dry runs of very
/// long timelines stay cheap.
pub fn cue_regions(
    intervals: &[Range<usize>],
    sample_rate: u32,
    frames: usize,
    params: &DuckParams,
) -> Vec<Range<usize>> {
    let blocks = block_count(sample_rate, frames);
    let Some(last_block) = blocks.checked_sub(1) else {
        return Vec::new(); // empty timeline or zero sample rate
    };
    // Step 1: the blocks each interval intersects.
    let mut runs = union(
        intervals
            .iter()
            .filter_map(|iv| {
                let (s, e) = (iv.start.min(frames), iv.end.min(frames));
                (s < e).then(|| {
                    block_of(sample_rate, s).min(last_block)
                        ..block_of(sample_rate, e - 1).min(last_block) + 1
                })
            })
            .collect(),
    );
    // Step 2: bridge gaps shorter than the hold, then extend every run by it (clamped to the end).
    let hold = ms_to_blocks(params.hold_ms);
    if hold > 0 {
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(runs.len());
        for run in runs {
            match merged.last_mut() {
                Some(prev) if run.start - prev.end < hold => prev.end = run.end,
                _ => merged.push(run),
            }
        }
        for run in &mut merged {
            run.end = run.end.saturating_add(hold).min(blocks);
        }
        runs = union(merged);
    }
    // Step 3: every run starts `lookahead` earlier (clamped at 0).
    let ahead = ms_to_blocks(params.lookahead_ms);
    if ahead > 0 {
        runs = union(
            runs.into_iter()
                .map(|r| r.start.saturating_sub(ahead)..r.end)
                .collect(),
        );
    }
    runs.into_iter()
        .map(|r| {
            block_start(sample_rate, r.start).min(frames)
                ..block_start(sample_rate, r.end).min(frames)
        })
        .filter(|r| !r.is_empty())
        .collect()
}

/// Activity of the trigger tracks per 1 ms block over a timeline of `frames` frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    sample_rate: u32,
    frames: usize,
    active: Vec<bool>,
}

impl Activity {
    /// No activity at all.
    pub fn inactive(sample_rate: u32, frames: usize) -> Self {
        Activity {
            sample_rate,
            frames,
            active: vec![false; block_count(sample_rate, frames)],
        }
    }

    /// Frame range of block `b`, clamped to the timeline.
    fn block_frames(&self, b: usize) -> Range<usize> {
        block_start(self.sample_rate, b).min(self.frames)
            ..block_start(self.sample_rate, b + 1).min(self.frames)
    }

    /// The block containing frame `f`.
    fn block_of(&self, f: usize) -> usize {
        block_of(self.sample_rate, f)
    }

    /// Sidechain detection (DESIGN.md §4.5 step 1): the detector is mixed to mono (`(L + R)/2`), its
    /// level is the RMS over a 10 ms window centred on each block (clamped at the edges), and a block is
    /// active when that level is above `threshold_db` dBFS. Detector frames beyond `frames` are
    /// ignored; missing ones count as silence.
    pub fn from_sidechain(detector: &AudioBuffer, threshold_db: f64, frames: usize) -> Self {
        let mut activity = Activity::inactive(detector.sample_rate, frames);
        if activity.active.is_empty() || detector.num_channels() == 0 {
            return activity;
        }
        let mono = detector.to_mono();
        let x = &mono.channels[0][..mono.frames().min(frames)];
        // prefix[i] = Σ x[..i]² (f64 keeps long sums exact enough).
        let mut prefix = Vec::with_capacity(x.len() + 1);
        prefix.push(0.0f64);
        let mut acc = 0.0f64;
        for &s in x {
            acc += f64::from(s) * f64::from(s);
            prefix.push(acc);
        }
        let len = x.len();
        let window = ((DETECTOR_WINDOW_MS * f64::from(detector.sample_rate) / 1000.0).round()
            as usize)
            .max(1);
        let threshold_ms = 10f64.powf(threshold_db / 10.0); // mean-square threshold
        for b in 0..activity.active.len() {
            let r = activity.block_frames(b);
            let centre = (r.start + r.end) / 2;
            let lo = centre.saturating_sub(window / 2);
            let hi = (lo + window).min(frames);
            let (lo_c, hi_c) = (lo.min(len), hi.min(len));
            let count = hi - lo;
            if count == 0 || hi_c <= lo_c {
                continue;
            }
            let mean_sq = (prefix[hi_c] - prefix[lo_c]) / count as f64;
            activity.active[b] = mean_sq > threshold_ms;
        }
        activity
    }

    /// Cue mode (DESIGN.md §4.5 step 1): a block is active when it intersects any of the half-open
    /// frame `intervals` (unsorted and overlapping intervals are fine).
    pub fn from_intervals(intervals: &[Range<usize>], sample_rate: u32, frames: usize) -> Self {
        let mut activity = Activity::inactive(sample_rate, frames);
        let Some(last_block) = activity.active.len().checked_sub(1) else {
            return activity; // empty timeline or zero sample rate
        };
        for iv in intervals {
            let (s, e) = (iv.start.min(frames), iv.end.min(frames));
            if s >= e {
                continue;
            }
            let first = activity.block_of(s).min(last_block);
            let last = activity.block_of(e - 1).min(last_block);
            for flag in &mut activity.active[first..=last] {
                *flag = true;
            }
        }
        activity
    }

    /// Maximal runs of active blocks as block ranges.
    fn runs(&self) -> Vec<Range<usize>> {
        let mut runs = Vec::new();
        let mut b = 0;
        while b < self.active.len() {
            if !self.active[b] {
                b += 1;
                continue;
            }
            let start = b;
            while b < self.active.len() && self.active[b] {
                b += 1;
            }
            runs.push(start..b);
        }
        runs
    }

    fn set_runs(&mut self, runs: &[Range<usize>]) {
        self.active.iter_mut().for_each(|a| *a = false);
        let n = self.active.len();
        for r in runs {
            for flag in &mut self.active[r.start.min(n)..r.end.min(n)] {
                *flag = true;
            }
        }
    }

    /// Hold (DESIGN.md §4.5 step 2): gaps between active runs shorter than `hold_ms` become active, then
    /// every active run is extended by `hold_ms` at its end.
    pub fn hold(mut self, hold_ms: f64) -> Self {
        let hold = ms_to_blocks(hold_ms);
        if hold == 0 {
            return self;
        }
        let mut merged: Vec<Range<usize>> = Vec::new();
        for run in self.runs() {
            match merged.last_mut() {
                Some(prev) if run.start - prev.end < hold => prev.end = run.end,
                _ => merged.push(run),
            }
        }
        for run in &mut merged {
            run.end += hold;
        }
        self.set_runs(&merged);
        self
    }

    /// Lookahead (DESIGN.md §4.5 step 3): every active run starts `lookahead_ms` earlier (clamped at 0).
    pub fn lookahead(mut self, lookahead_ms: f64) -> Self {
        let ahead = ms_to_blocks(lookahead_ms);
        if ahead == 0 {
            return self;
        }
        let runs: Vec<Range<usize>> = self
            .runs()
            .into_iter()
            .map(|r| r.start.saturating_sub(ahead)..r.end)
            .collect();
        self.set_runs(&runs);
        self
    }

    /// Active regions as half-open frame ranges, sorted and non-overlapping.
    pub fn regions(&self) -> Vec<Range<usize>> {
        self.runs()
            .into_iter()
            .map(|r| self.block_frames(r.start).start..self.block_frames(r.end - 1).end)
            .filter(|r| !r.is_empty())
            .collect()
    }

    /// True when no block is active.
    pub fn is_silent(&self) -> bool {
        !self.active.iter().any(|&a| a)
    }

    /// Number of 1 ms blocks.
    pub fn blocks(&self) -> usize {
        self.active.len()
    }

    /// Whether block `b` is active (`false` past the end).
    pub fn is_active(&self, b: usize) -> bool {
        self.active.get(b).copied().unwrap_or(false)
    }

    /// Timeline length in frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Sample rate of the timeline.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// The gain curve of one ducked track (DESIGN.md §4.5 steps 4–5).
#[derive(Debug, Clone, PartialEq)]
pub struct DuckEnvelope {
    activity: Activity,
    depth_db: f64,
    /// dB per frame going down (attack) and up (release); infinite for a zero-length ramp.
    attack_step: f64,
    release_step: f64,
}

impl DuckEnvelope {
    /// Build the envelope from activity that already had [`Activity::hold`] and
    /// [`Activity::lookahead`] applied. Uses `reduction_db`, `attack_ms` and `release_ms` of `params`.
    pub fn new(activity: Activity, params: &DuckParams) -> Self {
        let depth_db = if params.reduction_db.is_finite() {
            params.reduction_db.abs()
        } else {
            0.0
        };
        let step = |ms: f64| {
            let frames = ms * f64::from(activity.sample_rate) / 1000.0;
            if frames.is_finite() && frames > 0.0 {
                depth_db / frames
            } else {
                f64::INFINITY
            }
        };
        DuckEnvelope {
            attack_step: step(params.attack_ms),
            release_step: step(params.release_ms),
            depth_db,
            activity,
        }
    }

    /// Full sidechain pipeline: detect on `detector` (pre-fader trigger mix), hold, lookahead, envelope.
    pub fn sidechain(detector: &AudioBuffer, frames: usize, params: &DuckParams) -> Self {
        let activity = Activity::from_sidechain(detector, params.threshold_db, frames)
            .hold(params.hold_ms)
            .lookahead(params.lookahead_ms);
        DuckEnvelope::new(activity, params)
    }

    /// Full cue-mode pipeline from trigger item frame intervals.
    pub fn cue(
        intervals: &[Range<usize>],
        sample_rate: u32,
        frames: usize,
        params: &DuckParams,
    ) -> Self {
        let activity = Activity::from_intervals(intervals, sample_rate, frames)
            .hold(params.hold_ms)
            .lookahead(params.lookahead_ms);
        DuckEnvelope::new(activity, params)
    }

    /// Positive attenuation depth in dB.
    pub fn depth_db(&self) -> f64 {
        self.depth_db
    }

    /// The activity the envelope follows.
    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Active regions (after hold and lookahead) as frame ranges, for reports.
    pub fn regions(&self) -> Vec<Range<usize>> {
        self.activity.regions()
    }

    /// Per-frame gain in dB (≤ 0), one value per timeline frame. The gain starts at 0 dB and moves
    /// toward each block's target (−depth when active, else 0) by at most the attack (down) or release
    /// (up) slope per frame.
    pub fn gains_db(&self) -> impl Iterator<Item = f64> + '_ {
        let mut g = 0.0f64;
        let (depth, down, up) = (self.depth_db, self.attack_step, self.release_step);
        (0..self.activity.blocks())
            .flat_map(move |b| {
                let target = if self.activity.is_active(b) {
                    -depth
                } else {
                    0.0
                };
                self.activity.block_frames(b).map(move |_| target)
            })
            .map(move |target| {
                if g > target {
                    g = (g - down).max(target);
                } else if g < target {
                    g = (g + up).min(target);
                }
                // Snap accumulated rounding error so a ramp ends exactly after attack/release frames.
                if (g - target).abs() < RAMP_SNAP_DB {
                    g = target;
                }
                g
            })
    }

    /// Multiply `stem` by the envelope (every channel).
    ///
    /// Errors when the stem's sample rate or length differs from the envelope's timeline, or its
    /// channels are ragged.
    pub fn apply(&self, stem: &mut AudioBuffer) -> Result<(), DspError> {
        let ragged = stem.channels.iter().any(|c| c.len() != stem.frames());
        if ragged
            || stem.sample_rate != self.activity.sample_rate
            || stem.frames() != self.activity.frames
        {
            return Err(DspError::InvalidInput(format!(
                "ducking envelope is {} frames at {} Hz but the stem is {} frames at {} Hz",
                self.activity.frames,
                self.activity.sample_rate,
                stem.frames(),
                stem.sample_rate
            )));
        }
        if self.depth_db == 0.0 || self.activity.is_silent() {
            return Ok(());
        }
        let full = (10f64.powf(-self.depth_db / 20.0)) as f32;
        for (i, g_db) in self.gains_db().enumerate() {
            let g = if g_db == 0.0 {
                continue;
            } else if g_db == -self.depth_db {
                full
            } else {
                10f64.powf(g_db / 20.0) as f32
            };
            for ch in &mut stem.channels {
                ch[i] *= g;
            }
        }
        Ok(())
    }

    /// The envelope sampled at `points_per_second` (reports and the preview): point `k` is the gain in
    /// dB at frame `⌊k·sr/pps⌋`, rounded to 0.01 dB. `⌈frames·pps/sr⌉` points.
    pub fn sample_gain_db(&self, points_per_second: u32) -> Vec<f64> {
        let (sr, frames) = (u128::from(self.activity.sample_rate), self.activity.frames);
        if points_per_second == 0 || sr == 0 || frames == 0 {
            return Vec::new();
        }
        let pps = u128::from(points_per_second);
        let points = (frames as u128 * pps).div_ceil(sr) as usize;
        let mut out = Vec::with_capacity(points);
        let mut next = 0usize;
        for (i, g) in self.gains_db().enumerate() {
            while out.len() < points && (out.len() as u128 * sr / pps) as usize == i {
                out.push(round_db(g));
            }
            next = i;
        }
        debug_assert!(out.len() == points || next + 1 == frames);
        out
    }
}

/// Round to 0.01 dB, normalizing −0.0 to 0.0.
fn round_db(g: f64) -> f64 {
    let r = (g * 100.0).round() / 100.0;
    if r == 0.0 { 0.0 } else { r }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Frames of the millisecond range `[a, b)` at 48 kHz.
    fn ms(a: usize, b: usize) -> Range<usize> {
        a * 48..b * 48
    }

    fn params(depth: f64, attack: f64, release: f64, hold: f64, lookahead: f64) -> DuckParams {
        DuckParams {
            reduction_db: depth,
            attack_ms: attack,
            release_ms: release,
            hold_ms: hold,
            lookahead_ms: lookahead,
            threshold_db: -40.0,
        }
    }

    fn burst(rate: u32, total_ms: usize, on: &[(usize, usize)], amp: f32) -> AudioBuffer {
        let fpm = rate as usize / 1000;
        let mut x = vec![0.0f32; total_ms * fpm];
        for &(a, b) in on {
            for (i, s) in x[a * fpm..b * fpm].iter_mut().enumerate() {
                *s = amp * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate as f32).sin();
            }
        }
        AudioBuffer::mono(rate, x)
    }

    #[test]
    fn block_geometry_at_44k1() {
        let a = Activity::inactive(44_100, 44_101);
        assert_eq!(a.blocks(), 1001);
        assert_eq!(a.block_frames(0), 0..44);
        assert_eq!(a.block_frames(1), 44..88);
        assert_eq!(a.block_frames(9), 396..441);
        assert_eq!(a.block_frames(1000), 44_100..44_101);
        for f in [0usize, 43, 44, 87, 88, 440, 441, 44_099, 44_100] {
            let b = a.block_of(f);
            assert!(a.block_frames(b).contains(&f), "frame {f} in block {b}");
        }
    }

    #[test]
    fn sidechain_threshold() {
        // -20 dBFS-ish burst from 100 to 300 ms; a -60 dBFS burst from 500 to 600 ms.
        let mut det = burst(SR, 1000, &[(100, 300)], 0.1);
        let quiet = burst(SR, 1000, &[(500, 600)], 0.001);
        for (a, b) in det.channels[0].iter_mut().zip(&quiet.channels[0]) {
            *a += b;
        }
        let act = Activity::from_sidechain(&det, -40.0, det.frames());
        let regions = act.regions();
        assert_eq!(regions.len(), 1, "{regions:?}");
        // The centred 10 ms window reacts at most ~5 ms around the true edges.
        let (s, e) = (regions[0].start as i64, regions[0].end as i64);
        assert!(
            (s - 4800).abs() <= 5 * 48 && (e - 14_400).abs() <= 5 * 48,
            "{regions:?}"
        );
        // Lower threshold also sees the quiet burst.
        assert_eq!(
            Activity::from_sidechain(&det, -70.0, det.frames())
                .regions()
                .len(),
            2
        );
        // Stereo detector: (L+R)/2 — opposite-phase channels cancel.
        let l = det.channels[0].clone();
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        let st = AudioBuffer::new(SR, vec![l, r]);
        assert!(Activity::from_sidechain(&st, -40.0, st.frames()).is_silent());
        // Longer timeline than the detector: the rest is silence.
        assert_eq!(
            Activity::from_sidechain(&det, -40.0, 2 * det.frames()).blocks(),
            2000
        );
    }

    #[test]
    fn cue_intervals_hold_and_lookahead() {
        let fpm = 48;
        let act = Activity::from_intervals(
            &[ms(1000, 1500), ms(1600, 2000), ms(2600, 2700)],
            SR,
            4000 * fpm,
        );
        assert_eq!(
            act.regions(),
            vec![48_000..72_000, 76_800..96_000, 124_800..129_600]
        );
        // Hold 250 ms bridges the 100 ms gap but not the 600 ms one, and extends each run by 250 ms.
        let held = act.clone().hold(250.0);
        assert_eq!(
            held.regions(),
            vec![48_000..(2250 * fpm), (2600 * fpm)..(2950 * fpm)]
        );
        // Lookahead 80 ms moves starts earlier.
        let ahead = held.lookahead(80.0);
        assert_eq!(
            ahead.regions(),
            vec![(920 * fpm)..(2250 * fpm), (2520 * fpm)..(2950 * fpm)]
        );
        // Lookahead is clamped at 0 and hold at the end of the timeline.
        let edge = Activity::from_intervals(&[ms(10, 20), ms(3990, 4000)], SR, 4000 * fpm)
            .hold(250.0)
            .lookahead(80.0);
        assert_eq!(
            edge.regions(),
            vec![0..(270 * fpm), (3910 * fpm)..(4000 * fpm)]
        );
        // Partial-block intersection activates the block.
        let one = Activity::from_intervals(&[47..49, 47..48], SR, 480);
        assert_eq!(one.regions(), vec![0..96]);
    }

    #[test]
    fn degenerate_timelines() {
        let none = Activity::from_intervals(&[ms(0, 10)], 0, 480);
        assert_eq!(none.blocks(), 0);
        assert!(none.regions().is_empty());
        let empty = Activity::from_intervals(&[ms(0, 10)], SR, 0);
        assert!(empty.is_silent());
        let env = DuckEnvelope::new(empty, &DuckParams::default());
        assert_eq!(env.gains_db().count(), 0);
        let silent_detector = AudioBuffer::new(SR, vec![]);
        assert!(Activity::from_sidechain(&silent_detector, -40.0, 480).is_silent());
        // Zero depth: the envelope is flat even when active.
        let flat = DuckEnvelope::cue(
            &[ms(0, 10)],
            SR,
            480,
            &DuckParams {
                reduction_db: 0.0,
                ..DuckParams::default()
            },
        );
        assert!(flat.gains_db().all(|g| g == 0.0));
    }

    #[test]
    fn exact_attack_and_release_slopes() {
        let fpm = 48usize;
        let act = Activity::from_intervals(&[ms(1000, 2000)], SR, 3000 * fpm);
        let env = DuckEnvelope::new(act, &params(-18.0, 80.0, 400.0, 0.0, 0.0));
        let g: Vec<f64> = env.gains_db().collect();
        assert_eq!(g.len(), 3000 * fpm);
        let start = 1000 * fpm;
        let down = 18.0 / (80.0 * 48.0);
        assert_eq!(g[start - 1], 0.0);
        for k in 0..80 * fpm {
            assert!(
                (g[start + k] + down * (k + 1) as f64).abs() < 1e-9,
                "attack frame {k}"
            );
        }
        assert_eq!(
            g[start + 80 * fpm - 1],
            -18.0,
            "full depth exactly after attack_ms"
        );
        let end = 2000 * fpm;
        assert_eq!(g[end - 1], -18.0);
        let up = 18.0 / (400.0 * 48.0);
        for k in 0..400 * fpm {
            assert!(
                (g[end + k] - (-18.0 + up * (k + 1) as f64)).abs() < 1e-9,
                "release frame {k}"
            );
        }
        assert_eq!(
            g[end + 400 * fpm - 1],
            0.0,
            "back to 0 dB exactly after release_ms"
        );
        assert!(g[end + 400 * fpm..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn lookahead_ducks_fully_before_speech() {
        let fpm = 48usize;
        let p = DuckParams {
            lookahead_ms: 80.0,
            hold_ms: 0.0,
            ..DuckParams::default()
        };
        let env = DuckEnvelope::cue(&[ms(1000, 1500)], SR, 2000 * fpm, &p);
        let g: Vec<f64> = env.gains_db().collect();
        assert_eq!(g[1000 * fpm], -15.0, "fully ducked when speech begins");
        assert_eq!(g[920 * fpm - 1], 0.0, "ramp starts at the lookahead");
        assert!(g[920 * fpm] < 0.0);
    }

    #[test]
    fn zero_attack_steps_instantly_and_apply_scales_all_channels() {
        let fpm = 48usize;
        let act = Activity::from_intervals(&[ms(10, 20)], SR, 40 * fpm);
        let env = DuckEnvelope::new(act, &params(20.0, 0.0, 0.0, 0.0, 0.0));
        let mut stem = AudioBuffer::new(SR, vec![vec![1.0; 40 * fpm], vec![-0.5; 40 * fpm]]);
        env.apply(&mut stem).unwrap();
        assert_eq!(stem.channels[0][10 * fpm - 1], 1.0);
        assert!((stem.channels[0][10 * fpm] - 0.1).abs() < 1e-7);
        assert!((stem.channels[1][15 * fpm] + 0.05).abs() < 1e-7);
        assert_eq!(stem.channels[0][20 * fpm], 1.0);
        let mut wrong = AudioBuffer::silent(SR, 1, 10);
        assert!(env.apply(&mut wrong).is_err());
    }

    #[test]
    fn sidechain_end_to_end_with_hold_bridging() {
        // Two words 150 ms apart: a 250 ms hold keeps the bed down between them.
        let det = burst(SR, 3000, &[(500, 900), (1050, 1500)], 0.2);
        let p = DuckParams::default();
        let env = DuckEnvelope::sidechain(&det, det.frames(), &p);
        let regions = env.regions();
        assert_eq!(regions.len(), 1, "{regions:?}");
        let g: Vec<f64> = env.gains_db().collect();
        assert_eq!(g[975 * 48], -15.0, "held in the gap");
        // Released after the hold: last activity ≈1500 ms + 250 ms hold, then a 400 ms release.
        assert_eq!(g[2200 * 48], 0.0);
        assert!(g[1800 * 48] < 0.0);
    }

    #[test]
    fn sampled_envelope() {
        let fpm = 48usize;
        let env = DuckEnvelope::new(
            Activity::from_intervals(&[ms(100, 200)], SR, 300 * fpm),
            &params(12.0, 20.0, 40.0, 0.0, 0.0),
        );
        let pts = env.sample_gain_db(100); // one point per 10 ms
        assert_eq!(pts.len(), 30);
        assert_eq!(pts[9], 0.0);
        assert_eq!(
            pts[10],
            round_db(-12.0 / (20.0 * 48.0)),
            "first frame of the attack"
        );
        assert_eq!(pts[11], -6.01, "481 attack steps of 0.0125 dB");
        assert_eq!(pts[12], -12.0);
        assert_eq!(pts[20], -11.99, "first release frame");
        assert_eq!(pts[24], 0.0, "release complete after 40 ms");
        assert!(pts.iter().all(|v| *v <= 0.0));
        assert!(
            DuckEnvelope::new(Activity::inactive(SR, 0), &DuckParams::default())
                .sample_gain_db(50)
                .is_empty()
        );
        // Points that do not divide the rate evenly.
        let env = DuckEnvelope::new(Activity::inactive(44_100, 44_100), &DuckParams::default());
        assert_eq!(env.sample_gain_db(50).len(), 50);
        assert_eq!(env.sample_gain_db(30).len(), 30);
    }

    /// `cue_regions` returns exactly what the block-flag pipeline returns, on random interval sets
    /// (words, sentences, empty, reversed, overlapping, unsorted and out-of-range intervals) with random
    /// hold and lookahead, at block geometries of 48 frames (48 kHz) and 44/45 frames (44.1 kHz).
    #[test]
    fn cue_regions_match_the_envelope() {
        let mut rng = crate::quantize::XorShift64::new(0x00D0_C4ED);
        let mut below = |n: u64| if n == 0 { 0 } else { rng.next_u64() % n };
        let (mut cases, mut active, mut merged) = (0, 0, 0);
        for rate in [48_000u32, 44_100, 22_050, 8000] {
            for case in 0..500u64 {
                let total_ms = 1 + below(4000);
                let frames = (total_ms * u64::from(rate) / 1000 + below(50)) as usize;
                let intervals: Vec<Range<usize>> = (0..below(12))
                    .map(|_| {
                        let start = below(frames as u64 + 200) as usize;
                        let len = match below(4) {
                            0 => 0,
                            1 => below(100),
                            2 => below(u64::from(rate) / 2),
                            _ => below(frames as u64 + 1),
                        } as usize;
                        if below(10) == 0 {
                            start..start.saturating_sub(len) // reversed: empty
                        } else {
                            start..start + len
                        }
                    })
                    .collect();
                let pick = |table: &[f64], r: u64, random: f64| {
                    table.get(r as usize).copied().unwrap_or(random)
                };
                let p = DuckParams {
                    hold_ms: pick(
                        &[
                            0.0,
                            0.4,
                            0.5,
                            1.0,
                            1.5,
                            3.0,
                            44.0,
                            45.0,
                            250.0,
                            1000.0,
                            -5.0,
                            f64::NAN,
                        ],
                        below(16),
                        below(60_000) as f64 / 100.0,
                    ),
                    lookahead_ms: pick(
                        &[0.0, 0.5, 1.0, 80.0, -1.0],
                        below(8),
                        below(30_000) as f64 / 100.0,
                    ),
                    ..DuckParams::default()
                };
                let want = DuckEnvelope::cue(&intervals, rate, frames, &p).regions();
                assert_eq!(
                    cue_regions(&intervals, rate, frames, &p),
                    want,
                    "case {case}: {rate} Hz, {frames} frames, {intervals:?}, {p:?}"
                );
                cases += 1;
                active += usize::from(!want.is_empty());
                merged += usize::from(want.len() > 1);
            }
        }
        // The cases are not trivial: most have regions, many several.
        assert!(
            active * 10 > cases * 7 && merged * 10 > cases * 2,
            "{active}, {merged} of {cases}"
        );
    }

    #[test]
    fn cue_regions_edge_cases_and_huge_timelines() {
        let p = DuckParams::default();
        for (intervals, rate, frames) in [
            (vec![ms(0, 10)], 0u32, 480usize),
            (vec![ms(0, 10)], SR, 0),
            (vec![], SR, 480),
            (vec![ms(20, 30)], SR, 480), // entirely after the end
            (vec![0..1, 479..480], SR, 480),
        ] {
            assert_eq!(
                cue_regions(&intervals, rate, frames, &p),
                DuckEnvelope::cue(&intervals, rate, frames, &p).regions(),
                "{intervals:?} at {rate} Hz, {frames} frames"
            );
        }
        // A 10^13 ms timeline (no per-block state could exist): 1000–2000 ms with the default 250 ms
        // hold and 80 ms lookahead is active over 920–2250 ms.
        let huge = 10usize.pow(13) * 48;
        assert_eq!(
            cue_regions(&[ms(1000, 2000)], SR, huge, &p),
            vec![ms(920, 2250)]
        );
        let late = huge - 48_000;
        assert_eq!(
            cue_regions(std::slice::from_ref(&(late..huge + 5)), SR, huge, &p),
            vec![late - 80 * 48..huge],
            "clamped to the end"
        );
    }
}
