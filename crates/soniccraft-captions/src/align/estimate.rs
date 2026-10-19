//! Word-timing estimation without provider timings (DESIGN §5.4).
//!
//! Given the speech regions detected in a cue's (trimmed) audio, the text is split into phrases at
//! punctuation, phrase boundaries are assigned to the silence gaps between regions that best match the
//! proportional estimate (a monotonic dynamic-programming alignment), and words are distributed inside
//! each resulting segment proportionally to their syllable-based weight, in *speech time* (gaps the
//! segment contains are skipped). Without regions (dry run) words are distributed over the whole span,
//! with short pause slots after punctuation.

use crate::model::{Interval, Word};
use crate::text::{BreakStrength, Token, tokenize};

/// Estimate word timings for `display_text` within `span`.
///
/// `regions` are the speech regions detected in the cue audio (same time base as `span`; DESIGN §5.4:
/// gaps of at least 120 ms below -45 dBFS separate them). They are sorted, merged and clipped to `span`.
/// An empty slice means "no audio information" (dry run): words are then spread over the whole span.
///
/// The result has one [`Word`] per [`crate::text::tokenize`] token, in text order, with
/// non-decreasing, non-overlapping times inside `span`.
pub fn estimate(display_text: &str, span: Interval, regions: &[Interval]) -> Vec<Word> {
    let tokens = tokenize(display_text);
    let times = distribute(&tokens, span, regions);
    tokens
        .into_iter()
        .zip(times)
        .map(|(t, (s, e))| Word::new(t.text, s, e))
        .collect()
}

/// Pause slot (in syllable-weight units) after a phrase break, used only without audio information.
fn pause_weight(strength: BreakStrength) -> f64 {
    match strength {
        BreakStrength::Sentence => 1.5,
        BreakStrength::Clause => 1.0,
        BreakStrength::Comma => 0.5,
        BreakStrength::None => 0.0,
    }
}

/// Cost (in units of mean speech time per token) of leaving a phrase boundary without a gap.
fn skip_boundary_cost(strength: BreakStrength) -> f64 {
    match strength {
        BreakStrength::Sentence => 2.5,
        BreakStrength::Clause => 1.5,
        BreakStrength::Comma => 1.0,
        BreakStrength::None => 0.0,
    }
}

/// Cost (in units of mean speech time per token) of a gap that does not end a phrase: longer gaps are
/// more likely to be phrase boundaries.
fn skip_gap_cost(gap_ms: f64) -> f64 {
    (gap_ms / 400.0).clamp(0.25, 2.0)
}

/// Above this many DP cells the boundary/gap assignment falls back to a greedy pass.
const MAX_ASSIGNMENT_CELLS: usize = 4_000_000;

/// Distribute `tokens` over `window` (see [`estimate`]); one `(start, end)` per token.
pub(crate) fn distribute(
    tokens: &[Token],
    window: Interval,
    regions: &[Interval],
) -> Vec<(f64, f64)> {
    let n = tokens.len();
    if n == 0 {
        return Vec::new();
    }
    let window = window.sanitized();
    let mut weights: Vec<f64> = tokens.iter().map(|t| t.weight.max(0.0)).collect();
    if weights.iter().all(|&w| w <= 0.0) {
        weights.fill(1.0);
    }
    if regions.is_empty() {
        return distribute_text_only(tokens, &weights, window);
    }
    let regions = clip_regions(regions, window);
    match regions.len() {
        0 if window.duration_ms() > 0.0 => place(&weights, &[window]),
        0 => vec![(window.start_ms, window.start_ms); n],
        1 => place(&weights, &regions),
        _ => distribute_segmented(tokens, &weights, &regions),
    }
}

/// Sort, clip to `window`, merge overlapping or touching regions and drop empty or non-finite ones.
pub(crate) fn clip_regions(regions: &[Interval], window: Interval) -> Vec<Interval> {
    let mut clipped: Vec<Interval> = regions
        .iter()
        .filter(|r| r.start_ms.is_finite() && r.end_ms.is_finite())
        .map(|r| Interval::new(r.start_ms.max(window.start_ms), r.end_ms.min(window.end_ms)))
        .filter(|r| r.end_ms > r.start_ms)
        .collect();
    clipped.sort_by(|a, b| {
        a.start_ms
            .total_cmp(&b.start_ms)
            .then(a.end_ms.total_cmp(&b.end_ms))
    });
    let mut merged: Vec<Interval> = Vec::with_capacity(clipped.len());
    for r in clipped {
        match merged.last_mut() {
            Some(last) if r.start_ms <= last.end_ms => last.end_ms = last.end_ms.max(r.end_ms),
            _ => merged.push(r),
        }
    }
    merged
}

/// No audio information: spread words (plus pause slots after punctuation) linearly over the window.
fn distribute_text_only(tokens: &[Token], weights: &[f64], window: Interval) -> Vec<(f64, f64)> {
    let n = tokens.len();
    let pauses: Vec<f64> = (0..n)
        .map(|i| {
            if i + 1 < n {
                pause_weight(tokens[i].break_after)
            } else {
                0.0
            }
        })
        .collect();
    let total: f64 = weights.iter().sum::<f64>() + pauses.iter().sum::<f64>();
    let duration = window.duration_ms();
    let at = |pos: f64| window.start_ms + pos / total * duration;
    let mut out = Vec::with_capacity(n);
    let mut pos = 0.0;
    for i in 0..n {
        let start = at(pos);
        pos += weights[i];
        let end = if i + 1 == n { window.end_ms } else { at(pos) };
        out.push((start, end.max(start)));
        pos += pauses[i];
    }
    out
}

/// Spread words over the speech time of `regions` (in order, skipping the gaps between them).
fn place(weights: &[f64], regions: &[Interval]) -> Vec<(f64, f64)> {
    let n = weights.len();
    let (Some(first), Some(last)) = (regions.first(), regions.last()) else {
        return vec![(0.0, 0.0); n];
    };
    let speech: f64 = regions.iter().map(Interval::duration_ms).sum();
    let total: f64 = weights.iter().sum();
    if total <= 0.0 || speech <= 0.0 {
        return vec![(first.start_ms, first.start_ms); n];
    }
    let mut out = Vec::with_capacity(n);
    let mut acc = 0.0;
    for (i, w) in weights.iter().enumerate() {
        let start = speech_to_time(regions, acc / total * speech, true);
        acc += w;
        let end = if i + 1 == n {
            last.end_ms
        } else {
            speech_to_time(regions, acc / total * speech, false)
        };
        out.push((start, end.max(start)));
    }
    out
}

/// Map a position in speech time (ms of speech since the first region's start) to real time. At an exact
/// region boundary a start maps to the next region's start and an end to the current region's end.
fn speech_to_time(regions: &[Interval], position: f64, is_start: bool) -> f64 {
    let mut acc = 0.0;
    for (k, r) in regions.iter().enumerate() {
        let d = r.duration_ms();
        let last = k + 1 == regions.len();
        if position < acc + d || last || (!is_start && position <= acc + d) {
            return r.start_ms + (position - acc).clamp(0.0, d);
        }
        acc += d;
    }
    regions.last().map_or(0.0, |r| r.end_ms)
}

/// A phrase boundary: after token `token`, expected at `position` ms of speech time.
#[derive(Debug, Clone, Copy)]
struct Boundary {
    token: usize,
    strength: BreakStrength,
    position: f64,
}

/// The silence after region `region`, at `position` ms of speech time, `duration` ms long.
#[derive(Debug, Clone, Copy)]
struct Gap {
    region: usize,
    position: f64,
    duration: f64,
}

/// Several regions: snap phrase boundaries to gaps, then place words segment by segment.
fn distribute_segmented(
    tokens: &[Token],
    weights: &[f64],
    regions: &[Interval],
) -> Vec<(f64, f64)> {
    let n = tokens.len();
    let total: f64 = weights.iter().sum();
    let speech: f64 = regions.iter().map(Interval::duration_ms).sum();
    let mut boundaries = Vec::new();
    let mut acc = 0.0;
    for (i, token) in tokens.iter().enumerate() {
        acc += weights[i];
        if i + 1 < n && token.break_after != BreakStrength::None {
            boundaries.push(Boundary {
                token: i,
                strength: token.break_after,
                position: acc / total * speech,
            });
        }
    }
    let mut gaps = Vec::with_capacity(regions.len().saturating_sub(1));
    let mut position = 0.0;
    for k in 0..regions.len() - 1 {
        position += regions[k].duration_ms();
        gaps.push(Gap {
            region: k,
            position,
            duration: regions[k + 1].start_ms - regions[k].end_ms,
        });
    }
    let unit = speech / n as f64;
    let pairs = assign_boundaries(&boundaries, &gaps, unit);

    let mut out = Vec::with_capacity(n);
    let (mut t0, mut r0) = (0, 0);
    let ends = pairs
        .iter()
        .map(|&(b, g)| (boundaries[b].token, gaps[g].region))
        .chain([(n - 1, regions.len() - 1)]);
    for (t_end, r_end) in ends {
        out.extend(place(&weights[t0..=t_end], &regions[r0..=r_end]));
        t0 = t_end + 1;
        r0 = r_end + 1;
    }
    out
}

/// Monotonic assignment of phrase boundaries to gaps minimizing the total cost: matching costs the
/// distance between expected and actual position; an unmatched boundary or gap costs its skip penalty.
/// Ties prefer matching. Returns `(boundary index, gap index)` pairs in increasing order.
fn assign_boundaries(boundaries: &[Boundary], gaps: &[Gap], unit: f64) -> Vec<(usize, usize)> {
    let (nb, ng) = (boundaries.len(), gaps.len());
    if nb == 0 || ng == 0 {
        return Vec::new();
    }
    let skip_b = |b: &Boundary| skip_boundary_cost(b.strength) * unit;
    let skip_g = |g: &Gap| skip_gap_cost(g.duration) * unit;
    let match_cost = |b: &Boundary, g: &Gap| (b.position - g.position).abs();
    let cells = (nb + 1).saturating_mul(ng + 1);
    if cells > MAX_ASSIGNMENT_CELLS {
        // Greedy fallback for pathological sizes: match each boundary to the first remaining gap whose
        // match is cheaper than skipping both.
        let mut pairs = Vec::new();
        let mut j = 0;
        for (i, b) in boundaries.iter().enumerate() {
            while j < ng
                && gaps[j].position < b.position
                && match_cost(b, &gaps[j]) >= skip_b(b) + skip_g(&gaps[j])
            {
                j += 1;
            }
            if j < ng && match_cost(b, &gaps[j]) < skip_b(b) + skip_g(&gaps[j]) {
                pairs.push((i, j));
                j += 1;
            }
        }
        return pairs;
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Step {
        Start,
        Match,
        SkipBoundary,
        SkipGap,
    }
    let width = ng + 1;
    let mut cost = vec![f64::INFINITY; cells];
    let mut step = vec![Step::Start; cells];
    cost[0] = 0.0;
    for i in 0..=nb {
        for j in 0..=ng {
            if i == 0 && j == 0 {
                continue;
            }
            let mut best = f64::INFINITY;
            let mut choice = Step::Start;
            if i > 0 && j > 0 {
                let c =
                    cost[(i - 1) * width + j - 1] + match_cost(&boundaries[i - 1], &gaps[j - 1]);
                if c < best {
                    best = c;
                    choice = Step::Match;
                }
            }
            if i > 0 {
                let c = cost[(i - 1) * width + j] + skip_b(&boundaries[i - 1]);
                if c < best {
                    best = c;
                    choice = Step::SkipBoundary;
                }
            }
            if j > 0 {
                let c = cost[i * width + j - 1] + skip_g(&gaps[j - 1]);
                if c < best {
                    best = c;
                    choice = Step::SkipGap;
                }
            }
            cost[i * width + j] = best;
            step[i * width + j] = choice;
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (nb, ng);
    while i > 0 || j > 0 {
        match step[i * width + j] {
            Step::Match => {
                pairs.push((i - 1, j - 1));
                i -= 1;
                j -= 1;
            }
            Step::SkipBoundary => i -= 1,
            Step::SkipGap => j -= 1,
            Step::Start => break,
        }
    }
    pairs.reverse();
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(a: f64, b: f64) -> Interval {
        Interval::new(a, b)
    }

    fn assert_monotonic(words: &[Word], span: Interval) {
        for w in words {
            assert!(
                w.start_ms >= span.start_ms - 1e-9 && w.end_ms <= span.end_ms + 1e-9,
                "{w:?} outside {span:?}"
            );
            assert!(w.end_ms >= w.start_ms, "{w:?}");
        }
        for pair in words.windows(2) {
            assert!(pair[1].start_ms >= pair[0].end_ms - 1e-9, "{pair:?}");
        }
    }

    #[test]
    fn text_only_spreads_over_span_with_pauses() {
        let span = iv(1000.0, 3000.0);
        let words = estimate("One two, three.", span, &[]);
        assert_eq!(
            words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["One", "two,", "three."]
        );
        assert_eq!(words[0].start_ms, 1000.0);
        assert_eq!(words[2].end_ms, 3000.0);
        // "two," is followed by a comma pause, "One" is not.
        assert_eq!(words[1].start_ms, words[0].end_ms);
        assert!(words[2].start_ms > words[1].end_ms);
        assert_monotonic(&words, span);
    }

    #[test]
    fn proportional_to_syllables() {
        let words = estimate("a development", iv(0.0, 1000.0), &[]);
        let short = words[0].duration_ms();
        let long = words[1].duration_ms();
        assert!((long / short - 4.0).abs() < 1e-9, "{short} {long}");
    }

    #[test]
    fn single_region_excludes_padding() {
        let span = iv(0.0, 2000.0);
        let words = estimate("Hello there friend", span, &[iv(100.0, 1900.0)]);
        assert_eq!(words[0].start_ms, 100.0);
        assert_eq!(words[2].end_ms, 1900.0);
        assert_monotonic(&words, span);
    }

    #[test]
    fn phrase_boundaries_snap_to_gaps() {
        // Two sentences; the speaker paused 600 ms between them, later than the proportional estimate.
        let span = iv(0.0, 4000.0);
        let regions = [iv(0.0, 1800.0), iv(2400.0, 3600.0)];
        let words = estimate("This is the first sentence. Then a second.", span, &regions);
        let first_sentence_end = words[4].end_ms;
        assert_eq!(words[4].text, "sentence.");
        assert_eq!(first_sentence_end, 1800.0);
        assert_eq!(words[5].start_ms, 2400.0);
        assert_eq!(words.last().map(|w| w.end_ms), Some(3600.0));
        assert_monotonic(&words, span);
    }

    #[test]
    fn gap_without_punctuation_is_absorbed() {
        let span = iv(0.0, 3000.0);
        let regions = [iv(0.0, 1000.0), iv(1150.0, 3000.0)];
        let words = estimate("one two three four five six", span, &regions);
        // No punctuation: the gap is not a segment boundary, so a word may straddle it, but every word
        // still lies within the span and speech time is split proportionally.
        assert_monotonic(&words, span);
        assert_eq!(words[0].start_ms, 0.0);
        assert_eq!(words[5].end_ms, 3000.0);
    }

    #[test]
    fn more_boundaries_than_gaps_and_vice_versa() {
        let span = iv(0.0, 5000.0);
        let words = estimate(
            "Alpha, beta, gamma, delta. Echo, foxtrot.",
            span,
            &[iv(0.0, 2000.0), iv(2600.0, 5000.0)],
        );
        assert_monotonic(&words, span);
        // The sentence end after "delta." beats the closer comma after "gamma," for the single long gap.
        assert_eq!(words[3].end_ms, 2000.0);
        assert_eq!(words[4].start_ms, 2600.0);
        // A single-letter "d." is an initial, not a sentence end.
        assert_eq!(tokenize("d. E")[0].break_after, BreakStrength::None);

        let regions = [
            iv(0.0, 500.0),
            iv(700.0, 1200.0),
            iv(1400.0, 1900.0),
            iv(2100.0, 2600.0),
        ];
        let words = estimate("Short sentence. Another one", span, &regions);
        assert_monotonic(&words, span);
        assert_eq!(words.last().map(|w| w.end_ms), Some(2600.0));
    }

    #[test]
    fn regions_are_clipped_and_merged() {
        let span = iv(1000.0, 2000.0);
        let words = estimate(
            "x y",
            span,
            &[
                iv(500.0, 1200.0),
                iv(1100.0, 1300.0),
                iv(1900.0, 2500.0),
                iv(f64::NAN, 1.0),
            ],
        );
        assert_monotonic(&words, span);
        assert_eq!(words[0].start_ms, 1000.0);
        assert_eq!(words[1].end_ms, 2000.0);
        let merged = clip_regions(
            &[iv(0.0, 10.0), iv(10.0, 20.0), iv(30.0, 40.0)],
            iv(0.0, 100.0),
        );
        assert_eq!(merged, [iv(0.0, 20.0), iv(30.0, 40.0)]);
    }

    #[test]
    fn regions_outside_window_fall_back_to_linear() {
        let words = estimate("x y", iv(0.0, 100.0), &[iv(500.0, 900.0)]);
        assert_eq!(words[0].start_ms, 0.0);
        assert_eq!(words[1].end_ms, 100.0);
    }

    #[test]
    fn degenerate_inputs() {
        assert!(estimate("", iv(0.0, 100.0), &[]).is_empty());
        let zero = estimate("a b", iv(500.0, 500.0), &[]);
        assert!(
            zero.iter()
                .all(|w| w.start_ms == 500.0 && w.end_ms == 500.0)
        );
        let inverted = estimate("a b", iv(500.0, 100.0), &[]);
        assert!(
            inverted
                .iter()
                .all(|w| w.start_ms == 500.0 && w.end_ms == 500.0)
        );
        let nan = estimate("a", iv(f64::NAN, f64::NAN), &[]);
        assert_eq!((nan[0].start_ms, nan[0].end_ms), (0.0, 0.0));
        let unspoken = estimate("…", iv(0.0, 300.0), &[]);
        assert_eq!((unspoken[0].start_ms, unspoken[0].end_ms), (0.0, 300.0));
    }

    #[test]
    fn deterministic() {
        let regions = [iv(0.0, 700.0), iv(900.0, 2000.0), iv(2300.0, 3100.0)];
        let a = estimate(
            "Hello, world. This is a test; of the system.",
            iv(0.0, 3200.0),
            &regions,
        );
        let b = estimate(
            "Hello, world. This is a test; of the system.",
            iv(0.0, 3200.0),
            &regions,
        );
        assert_eq!(a, b);
    }
}
