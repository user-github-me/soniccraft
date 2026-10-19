//! Mapping provider word timings onto display-text tokens (DESIGN §5.4).
//!
//! Providers time the words *they* saw (the spoken text, possibly normalized: `"1999"` may come back as
//! `"nineteen ninety-nine"`, punctuation may be split off or dropped, case may differ), while subtitles
//! show the display text (possibly a `caption` override). The mapping works in three passes:
//!
//! 1. **Word anchors.** Display tokens and provider words are compared by [`crate::text::match_key`]
//!    (lowercase letters and digits only) and aligned with a longest-common-subsequence pass (a greedy
//!    resynchronizing pass for very long cues). Every equal pair becomes an anchor with the provider's
//!    exact timing.
//! 2. **Character matches.** Between two anchors, the remaining tokens and words are aligned on their key
//!    characters (longest-common-substring recursion), which handles different tokenizations:
//!    `"SQL"` ↔ `"S" "Q" "L"`, `"New" "York"` ↔ `"NewYork"`, `"AT&T"` ↔ `"A" "T" "and" "T"`. A word shared
//!    by several tokens is split proportionally to the matched characters.
//! 3. **Estimates.** Tokens still unmatched (numbers spoken as words, words the provider dropped) are
//!    estimated inside the time window between their matched neighbours — preferably the span of the
//!    provider words that matched nothing there — with the speech-region method of
//!    [`crate::align::estimate`](fn@crate::align::estimate). A window too short to hold them borrows
//!    time from the neighbouring tokens.
//!
//! The result always has one word per display token, with non-decreasing, non-overlapping times inside
//! the span.

use std::collections::BTreeMap;

use super::estimate::{clip_regions, distribute};
use super::{Alignment, AlignmentSource};
use crate::model::{Interval, Word};
use crate::text::{Token, match_key, tokenize};

/// Minimum plausible speech time per unit of token weight; shorter windows borrow from neighbours.
const MIN_MS_PER_WEIGHT: f64 = 50.0;
/// Word-level LCS table limit; larger inputs use the greedy resynchronizing alignment.
const MAX_LCS_CELLS: usize = 1_000_000;
/// Character-level matching limit per gap (product of the key lengths).
const MAX_CHAR_CELLS: usize = 250_000;
/// Look-ahead of the greedy resynchronizing alignment.
const RESYNC_WINDOW: usize = 32;

/// Map provider word timings onto the tokens of `display_text`.
///
/// * `provider_words`: words as timed by the TTS provider (any tokenization, any case, punctuation-only
///   entries allowed and ignored), in spoken order. Non-finite times are ignored; times are clamped to
///   `span` and made non-decreasing.
/// * `span`: the cue's audio span; `regions`: speech regions detected in it (may be empty). All three use
///   one time base (e.g. milliseconds from the start of the trimmed cue audio).
///
/// With no usable provider words, or when nothing matches, the result is exactly
/// [`crate::align::estimate`](fn@crate::align::estimate)`(display_text, span, regions)` with
/// [`AlignmentSource::Estimated`].
///
/// This also serves cues with a `caption` override: pass the caption as `display_text` and the timings of
/// the spoken text; words the two share keep provider timing, the rest is estimated in between.
pub fn map_provider_words(
    display_text: &str,
    provider_words: &[Word],
    span: Interval,
    regions: &[Interval],
) -> Alignment {
    let span = span.sanitized();
    let tokens = tokenize(display_text);
    let provider = sanitize_provider(provider_words, span);
    if tokens.is_empty() {
        let source = if provider.is_empty() {
            AlignmentSource::Estimated
        } else {
            AlignmentSource::Provider
        };
        return Alignment {
            words: Vec::new(),
            source,
            matched: 0,
        };
    }
    if provider.is_empty() {
        return estimated(&tokens, span, regions);
    }

    let mut matches: Vec<Option<TokenMatch>> = vec![None; tokens.len()];
    let mut used = vec![false; provider.len()];
    let token_keys: Vec<&str> = tokens.iter().map(|t| t.key.as_str()).collect();
    let provider_keys: Vec<&str> = provider.iter().map(|p| p.key.as_str()).collect();
    let anchors = align_keys(&token_keys, &provider_keys);
    for &(t, p) in &anchors {
        matches[t] = Some(TokenMatch {
            start: provider[p].start,
            end: provider[p].end,
            first: p,
            last: p,
            missing_prefix: false,
            missing_suffix: false,
        });
        used[p] = true;
    }
    // Character-level matching between consecutive anchors (and before the first / after the last).
    let mut lo = (0, 0);
    for &(t_hi, p_hi) in anchors
        .iter()
        .chain([(tokens.len(), provider.len())].iter())
    {
        if t_hi > lo.0 && p_hi > lo.1 {
            char_match(
                &tokens,
                lo.0..t_hi,
                &provider,
                lo.1..p_hi,
                &mut matches,
                &mut used,
            );
        }
        lo = (t_hi + 1, p_hi + 1);
    }

    let matched = matches.iter().filter(|m| m.is_some()).count();
    if matched == 0 {
        return estimated(&tokens, span, regions);
    }
    absorb_orphans(&mut matches, &mut used, &provider);
    let mut times: Vec<(f64, f64)> = matches
        .iter()
        .map(|m| m.map_or((f64::NAN, f64::NAN), |m| (m.start, m.end)))
        .collect();
    let speech = speech_bounds(span, regions);
    let mut i = 0;
    while i < tokens.len() {
        if matches[i].is_some() {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < tokens.len() && matches[i].is_none() {
            i += 1;
        }
        fill_run(
            &tokens,
            run_start..i,
            &matches,
            &provider,
            &used,
            speech,
            regions,
            &mut times,
        );
    }
    finalize(&mut times, span);
    let source = if matched == tokens.len() {
        AlignmentSource::Provider
    } else {
        AlignmentSource::Mixed
    };
    let words = tokens
        .into_iter()
        .zip(times)
        .map(|(t, (s, e))| Word::new(t.text, s, e))
        .collect();
    Alignment {
        words,
        source,
        matched,
    }
}

/// A provider word with a non-empty key and sanitized times.
#[derive(Debug, Clone)]
struct ProviderWord {
    start: f64,
    end: f64,
    key: String,
}

/// Where a matched token's timing came from.
#[derive(Debug, Clone, Copy)]
struct TokenMatch {
    start: f64,
    end: f64,
    /// First and last provider word index the token touches.
    first: usize,
    last: usize,
    /// The start (end) of the token's key matched nothing: `"12.5%"` matched only `"percent"`.
    missing_prefix: bool,
    missing_suffix: bool,
}

fn estimated(tokens: &[Token], span: Interval, regions: &[Interval]) -> Alignment {
    let times = distribute(tokens, span, regions);
    let words = tokens
        .iter()
        .zip(times)
        .map(|(t, (s, e))| Word::new(t.text.clone(), s, e))
        .collect();
    Alignment {
        words,
        source: AlignmentSource::Estimated,
        matched: 0,
    }
}

fn sanitize_provider(words: &[Word], span: Interval) -> Vec<ProviderWord> {
    let mut out = Vec::with_capacity(words.len());
    let mut last_start = span.start_ms;
    for w in words {
        if !w.start_ms.is_finite() || !w.end_ms.is_finite() {
            continue;
        }
        let key = match_key(&w.text);
        if key.is_empty() {
            continue;
        }
        let start = w.start_ms.clamp(span.start_ms, span.end_ms).max(last_start);
        let end = w.end_ms.clamp(span.start_ms, span.end_ms).max(start);
        last_start = start;
        out.push(ProviderWord { start, end, key });
    }
    out
}

/// First speech region start and last region end inside `span` (the span itself without regions).
fn speech_bounds(span: Interval, regions: &[Interval]) -> (f64, f64) {
    let clipped = clip_regions(regions, span);
    match (clipped.first(), clipped.last()) {
        (Some(first), Some(last)) => (first.start_ms, last.end_ms),
        _ => (span.start_ms, span.end_ms),
    }
}

// ---- pass 1: word anchors -----------------------------------------------------------------------------

/// Pairs `(i, j)` with `a[i] == b[j]`, strictly increasing in both indices.
fn align_keys<'k>(a: &[&'k str], b: &[&'k str]) -> Vec<(usize, usize)> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    // Intern keys so comparisons are integer compares.
    fn intern<'k>(ids: &mut BTreeMap<&'k str, u32>, key: &'k str) -> u32 {
        let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        *ids.entry(key).or_insert(next)
    }
    let mut ids: BTreeMap<&'k str, u32> = BTreeMap::new();
    let a_ids: Vec<u32> = a.iter().map(|&s| intern(&mut ids, s)).collect();
    let b_ids: Vec<u32> = b.iter().map(|&s| intern(&mut ids, s)).collect();
    if (a.len() + 1).saturating_mul(b.len() + 1) <= MAX_LCS_CELLS {
        lcs_pairs(&a_ids, &b_ids)
    } else {
        greedy_pairs(&a_ids, &b_ids)
    }
}

/// Longest common subsequence, walking forward and taking every equal pair (earliest matches first).
fn lcs_pairs(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let width = m + 1;
    // suffix[i * width + j] = LCS length of a[i..] and b[j..].
    let mut suffix = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            suffix[i * width + j] = if a[i] == b[j] {
                suffix[(i + 1) * width + j + 1] + 1
            } else {
                suffix[(i + 1) * width + j].max(suffix[i * width + j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if suffix[(i + 1) * width + j] >= suffix[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Linear-time alignment for very long inputs: advance in lockstep and, on a mismatch, resynchronize at
/// the nearest pair of positions (within [`RESYNC_WINDOW`]) where two consecutive keys agree.
fn greedy_pairs(a: &[u32], b: &[u32]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let confirmed =
        |x: usize, y: usize| a[x] == b[y] && (x + 1 >= n || y + 1 >= m || a[x + 1] == b[y + 1]);
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
            continue;
        }
        let mut resync = None;
        'search: for distance in 1..=2 * RESYNC_WINDOW {
            for di in 0..=distance.min(RESYNC_WINDOW) {
                let dj = distance - di;
                if dj <= RESYNC_WINDOW && i + di < n && j + dj < m && confirmed(i + di, j + dj) {
                    resync = Some((di, dj));
                    break 'search;
                }
            }
        }
        match resync {
            Some((di, dj)) => {
                i += di;
                j += dj;
            }
            None => {
                i += 1;
                j += 1;
            }
        }
    }
    pairs
}

// ---- pass 2: character matches ------------------------------------------------------------------------

/// Align the key characters of `tokens[t_range]` and `provider[p_range]` and record matches for tokens
/// that touch at least one accepted block.
fn char_match(
    tokens: &[Token],
    t_range: std::ops::Range<usize>,
    provider: &[ProviderWord],
    p_range: std::ops::Range<usize>,
    matches: &mut [Option<TokenMatch>],
    used: &mut [bool],
) {
    // Flatten keys, remembering the owner of each character and the boundaries of each owner.
    let mut a: Vec<char> = Vec::new();
    let mut a_owner: Vec<usize> = Vec::new();
    let mut a_bounds: Vec<(usize, usize)> = Vec::new();
    for t in t_range.clone() {
        let start = a.len();
        for c in tokens[t].key.chars() {
            a.push(c);
            a_owner.push(t);
        }
        a_bounds.push((start, a.len()));
    }
    let mut b: Vec<char> = Vec::new();
    let mut b_owner: Vec<usize> = Vec::new();
    let mut b_offset: Vec<usize> = Vec::new();
    let mut b_bounds: Vec<(usize, usize)> = Vec::new();
    for p in p_range.clone() {
        let start = b.len();
        for (k, c) in provider[p].key.chars().enumerate() {
            b.push(c);
            b_owner.push(p);
            b_offset.push(k);
        }
        b_bounds.push((start, b.len()));
    }
    if a.is_empty() || b.is_empty() || a.len().saturating_mul(b.len()) > MAX_CHAR_CELLS {
        return;
    }
    let is_a_start = |i: usize| a_bounds.iter().any(|&(s, _)| s == i);
    let is_a_end = |i: usize| a_bounds.iter().any(|&(_, e)| e == i);
    let is_b_start = |j: usize| b_bounds.iter().any(|&(s, _)| s == j);
    let is_b_end = |j: usize| b_bounds.iter().any(|&(_, e)| e == j);
    // Total key length of the tokens (or words) a block touches.
    let touched = |bounds: &[(usize, usize)], from: usize, k: usize| -> usize {
        bounds
            .iter()
            .filter(|&&(s, e)| s < from + k && e > from)
            .map(|&(s, e)| e - s)
            .sum()
    };
    // A block is accepted when it covers whole keys on both sides ("pm" ↔ "p" "m"), or when it is at
    // least 3 characters long and covers at least half of what it touches on one side ("colour" ↔
    // "color", "Morgen" ↔ "morning", but not "running" ↔ "singing").
    let acceptable = |i: usize, j: usize, k: usize| {
        let whole = is_a_start(i) && is_a_end(i + k) && is_b_start(j) && is_b_end(j + k);
        let substantial =
            k >= 3 && (2 * k >= touched(&a_bounds, i, k) || 2 * k >= touched(&b_bounds, j, k));
        whole || substantial
    };

    let mut a_match: Vec<Option<usize>> = vec![None; a.len()];
    let mut stack = vec![(0, a.len(), 0, b.len())];
    while let Some((a0, a1, b0, b1)) = stack.pop() {
        if a0 >= a1 || b0 >= b1 {
            continue;
        }
        let (i, j, k) = longest_common_substring(&a[a0..a1], &b[b0..b1]);
        let (i, j) = (a0 + i, b0 + j);
        if k == 0 || !acceptable(i, j, k) {
            continue;
        }
        for d in 0..k {
            a_match[i + d] = Some(j + d);
        }
        stack.push((a0, i, b0, j));
        stack.push((i + k, a1, j + k, b1));
    }

    // Matched character range in `b` of each token (and whether its key's ends matched).
    let mut matched: Vec<(usize, usize, usize, bool, bool)> = Vec::new();
    for (local, &(s, e)) in a_bounds.iter().enumerate() {
        let hits: Vec<usize> = (s..e).filter_map(|i| a_match[i]).collect();
        if let (Some(&lo), Some(&hi)) = (hits.iter().min(), hits.iter().max()) {
            let missing_prefix = a_match[s].is_none();
            let missing_suffix = a_match[e - 1].is_none();
            matched.push((
                t_range.start + local,
                lo,
                hi,
                missing_prefix,
                missing_suffix,
            ));
        }
    }
    for (idx, &(t, lo, hi, missing_prefix, missing_suffix)) in matched.iter().enumerate() {
        let (first, last) = (b_owner[lo], b_owner[hi]);
        let shared_first = idx > 0 && b_owner[matched[idx - 1].2] == first;
        let shared_last = matched
            .get(idx + 1)
            .is_some_and(|next| b_owner[next.1] == last);
        let word_time = |p: usize, frac: f64| {
            let w = &provider[p];
            w.start + frac * (w.end - w.start)
        };
        let len_of = |p: usize| provider[p].key.chars().count().max(1) as f64;
        let start = if shared_first {
            word_time(first, b_offset[lo] as f64 / len_of(first))
        } else {
            provider[first].start
        };
        let end = if shared_last {
            word_time(last, (b_offset[hi] + 1) as f64 / len_of(last))
        } else {
            provider[last].end
        };
        matches[t] = Some(TokenMatch {
            start,
            end: end.max(start),
            first,
            last,
            missing_prefix,
            missing_suffix,
        });
        for flag in &mut used[first..=last] {
            *flag = true;
        }
    }
}

/// `(i, j, k)` such that `a[i..i + k] == b[j..j + k]` with `k` maximal; ties resolve to the earliest end
/// in `a`, then in `b`.
fn longest_common_substring(a: &[char], b: &[char]) -> (usize, usize, usize) {
    let mut prev = vec![0u32; b.len() + 1];
    let mut cur = vec![0u32; b.len() + 1];
    let (mut best_i, mut best_j, mut best_k) = (0, 0, 0usize);
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            cur[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1] + 1
            } else {
                0
            };
            let k = cur[j] as usize;
            if k > best_k {
                best_k = k;
                best_i = i - k;
                best_j = j - k;
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    (best_i, best_j, best_k)
}

/// Give provider words that nothing matched to an adjacent, partially matched token when no unmatched
/// display token sits between them: a token whose key start matched nothing absorbs the words before
/// it, one whose key end matched nothing the words after it (`"12.5%"` matched to `"percent"` absorbs
/// `"twelve point five"`; `"+5"` matched to `"plus"` absorbs `"five"`). Words next to complete matches
/// are extra words and stay unused.
fn absorb_orphans(
    matches: &mut [Option<TokenMatch>],
    used: &mut [bool],
    provider: &[ProviderWord],
) {
    let n = matches.len();
    for t in 0..=n {
        // The pair (t - 1, t) of neighbouring tokens; a missing side is the start or end of the cue.
        let left = match t.checked_sub(1).map(|l| (l, matches[l])) {
            Some((l, Some(m))) => Some((l, m)),
            Some((_, None)) => continue,
            None => None,
        };
        let right = match (t < n).then(|| (t, matches[t])) {
            Some((r, Some(m))) => Some((r, m)),
            Some((_, None)) => continue,
            None => None,
        };
        let p_lo = left.map_or(0, |(_, m)| m.last + 1);
        let p_hi = right.map_or(provider.len(), |(_, m)| m.first);
        let orphans: Vec<usize> = (p_lo..p_hi).filter(|&p| !used[p]).collect();
        let (Some(&first), Some(&last)) = (orphans.first(), orphans.last()) else {
            continue;
        };
        if let Some((r, m)) = right
            && m.missing_prefix
        {
            matches[r] = Some(TokenMatch {
                start: provider[first].start.min(m.start),
                first,
                missing_prefix: false,
                ..m
            });
        } else if let Some((l, m)) = left
            && m.missing_suffix
        {
            matches[l] = Some(TokenMatch {
                end: provider[last].end.max(m.end),
                last,
                missing_suffix: false,
                ..m
            });
        } else {
            continue;
        }
        for p in orphans {
            used[p] = true;
        }
    }
}

// ---- pass 3: estimates for unmatched runs ---------------------------------------------------------------

/// Time the unmatched tokens `run` between their matched neighbours.
#[allow(clippy::too_many_arguments)]
fn fill_run(
    tokens: &[Token],
    run: std::ops::Range<usize>,
    matches: &[Option<TokenMatch>],
    provider: &[ProviderWord],
    used: &[bool],
    speech: (f64, f64),
    regions: &[Interval],
    times: &mut [(f64, f64)],
) {
    let left = run.start.checked_sub(1);
    let right = (run.end < tokens.len()).then_some(run.end);
    let left_bound = left.map_or(speech.0, |l| times[l].1);
    let right_bound = right.map_or(speech.1, |r| times[r].0);

    // Provider words between the neighbours that nothing matched: most likely what was spoken here.
    let p_lo = left.and_then(|l| matches[l]).map_or(0, |m| m.last + 1);
    let p_hi = right
        .and_then(|r| matches[r])
        .map_or(provider.len(), |m| m.first);
    let mut window = (left_bound, right_bound.max(left_bound));
    if p_lo < p_hi {
        let orphans: Vec<&ProviderWord> = (p_lo..p_hi)
            .filter(|&p| !used[p])
            .map(|p| &provider[p])
            .collect();
        if let (Some(first), Some(last)) = (orphans.first(), orphans.last()) {
            let (start, end) = (first.start.max(left_bound), last.end.min(right_bound));
            if end > start {
                window = (start, end);
            }
        }
    }

    let weight: f64 = tokens[run.clone()].iter().map(|t| t.weight.max(1.0)).sum();
    if window.1 - window.0 >= MIN_MS_PER_WEIGHT * weight {
        let placed = distribute(
            &tokens[run.clone()],
            Interval::new(window.0, window.1),
            regions,
        );
        for (slot, t) in times[run].iter_mut().zip(placed) {
            *slot = t;
        }
        return;
    }
    // Too little room: re-estimate the run together with its neighbours over their combined span.
    let group = left.unwrap_or(run.start)..right.map_or(run.end, |r| r + 1);
    let start = left.map_or(speech.0.min(window.0), |l| times[l].0);
    let end = right
        .map_or(speech.1.max(window.1), |r| times[r].1)
        .max(start);
    let placed = distribute(&tokens[group.clone()], Interval::new(start, end), regions);
    for (slot, t) in times[group].iter_mut().zip(placed) {
        *slot = t;
    }
}

/// Make times finite, inside `span`, non-decreasing and non-overlapping.
fn finalize(times: &mut [(f64, f64)], span: Interval) {
    let mut prev_start = span.start_ms;
    for t in times.iter_mut() {
        let start = if t.0.is_finite() {
            t.0.clamp(span.start_ms, span.end_ms)
        } else {
            prev_start
        }
        .max(prev_start);
        let end = if t.1.is_finite() {
            t.1.clamp(span.start_ms, span.end_ms)
        } else {
            start
        }
        .max(start);
        *t = (start, end);
        prev_start = start;
    }
    for k in 1..times.len() {
        let next_start = times[k].0;
        let cur = &mut times[k - 1];
        if cur.1 > next_start {
            cur.1 = next_start.max(cur.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::estimate;

    fn iv(a: f64, b: f64) -> Interval {
        Interval::new(a, b)
    }

    fn w(text: &str, s: f64, e: f64) -> Word {
        Word::new(text, s, e)
    }

    fn check_invariants(words: &[Word], span: Interval) {
        for word in words {
            assert!(
                word.start_ms >= span.start_ms && word.end_ms <= span.end_ms,
                "{word:?} outside {span:?}"
            );
            assert!(word.end_ms >= word.start_ms, "{word:?}");
        }
        for pair in words.windows(2) {
            assert!(pair[1].start_ms >= pair[0].end_ms, "{pair:?}");
        }
    }

    #[test]
    fn exact_match_keeps_provider_times() {
        let provider = [w("Hello", 0.0, 400.0), w("world", 450.0, 900.0)];
        let a = map_provider_words("Hello, world!", &provider, iv(0.0, 1000.0), &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(a.matched, 2);
        assert_eq!(
            a.words,
            [w("Hello,", 0.0, 400.0), w("world!", 450.0, 900.0)]
        );
    }

    #[test]
    fn punctuation_and_case_differences() {
        // Kokoro-style punctuation tokens and different case/apostrophes.
        let provider = [
            w("DON'T", 0.0, 300.0),
            w(",", 300.0, 350.0),
            w("stop", 400.0, 700.0),
            w(".", 700.0, 720.0),
        ];
        let a = map_provider_words("Don’t stop.", &provider, iv(0.0, 800.0), &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(a.words, [w("Don’t", 0.0, 300.0), w("stop.", 400.0, 700.0)]);
    }

    #[test]
    fn split_and_merged_tokens() {
        let provider = [
            w("S", 0.0, 200.0),
            w("Q", 200.0, 400.0),
            w("L", 400.0, 600.0),
            w("rocks", 700.0, 1100.0),
        ];
        let a = map_provider_words("SQL rocks", &provider, iv(0.0, 1200.0), &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(a.words, [w("SQL", 0.0, 600.0), w("rocks", 700.0, 1100.0)]);

        let provider = [w("NewYork", 0.0, 700.0), w("city", 800.0, 1200.0)];
        let a = map_provider_words("New York city", &provider, iv(0.0, 1300.0), &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(a.words[0], w("New", 0.0, 300.0));
        assert_eq!(a.words[1], w("York", 300.0, 700.0));
        assert_eq!(a.words[2], w("city", 800.0, 1200.0));

        let provider = [
            w("A", 0.0, 100.0),
            w("T", 100.0, 200.0),
            w("and", 200.0, 400.0),
            w("T's", 400.0, 600.0),
            w("CEO", 650.0, 1000.0),
        ];
        let a = map_provider_words("AT&T's CEO", &provider, iv(0.0, 1000.0), &[]);
        assert_eq!(a.words, [w("AT&T's", 0.0, 600.0), w("CEO", 650.0, 1000.0)]);
    }

    #[test]
    fn numbers_spoken_differently() {
        let provider = [
            w("In", 0.0, 200.0),
            w("nineteen", 250.0, 700.0),
            w("ninety-nine", 700.0, 1200.0),
            w("we", 1300.0, 1450.0),
            w("won", 1450.0, 1800.0),
        ];
        let a = map_provider_words("In 1999 we won.", &provider, iv(0.0, 2000.0), &[]);
        assert_eq!(a.source, AlignmentSource::Mixed);
        assert_eq!(a.matched, 3);
        assert_eq!(a.words[1], w("1999", 250.0, 1200.0));
        check_invariants(&a.words, iv(0.0, 2000.0));
    }

    #[test]
    fn partially_matched_tokens_absorb_spoken_numbers() {
        let provider = [
            w("It", 0.0, 100.0),
            w("rose", 150.0, 400.0),
            w("twelve", 450.0, 800.0),
            w("point", 800.0, 1000.0),
            w("five", 1000.0, 1250.0),
            w("percent", 1250.0, 1700.0),
            w("today", 1800.0, 2200.0),
        ];
        let a = map_provider_words("It rose 12.5% today.", &provider, iv(0.0, 2300.0), &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(a.words[2], w("12.5%", 450.0, 1700.0));

        // At the start of the cue, and a missing suffix.
        let provider = [
            w("fifty", 0.0, 300.0),
            w("percent", 300.0, 700.0),
            w("of", 750.0, 850.0),
        ];
        let a = map_provider_words("50% of", &provider, iv(0.0, 900.0), &[]);
        assert_eq!(a.words[0], w("50%", 0.0, 700.0));
        let provider = [
            w("plus", 0.0, 200.0),
            w("five", 200.0, 500.0),
            w("degrees", 550.0, 1000.0),
        ];
        let a = map_provider_words("+5 degrees", &provider, iv(0.0, 1000.0), &[]);
        assert_eq!(a.words[0], w("+5", 0.0, 500.0));

        // Next to complete matches, extra words stay unused.
        let provider = [
            w("a", 0.0, 100.0),
            w("um", 100.0, 300.0),
            w("test", 300.0, 600.0),
        ];
        let a = map_provider_words("a test", &provider, iv(0.0, 600.0), &[]);
        assert_eq!(a.words, [w("a", 0.0, 100.0), w("test", 300.0, 600.0)]);
    }

    #[test]
    fn missing_provider_words_borrow_time() {
        // The provider skipped "dear": it gets a share of its neighbours' span.
        let provider = [w("Hello", 0.0, 500.0), w("world", 500.0, 1000.0)];
        let a = map_provider_words("Hello dear world", &provider, iv(0.0, 1000.0), &[]);
        assert_eq!(a.source, AlignmentSource::Mixed);
        assert_eq!(a.words[0].start_ms, 0.0);
        assert_eq!(a.words[2].end_ms, 1000.0);
        assert!(a.words[1].duration_ms() > 0.0);
        check_invariants(&a.words, iv(0.0, 1000.0));
    }

    #[test]
    fn extra_provider_words_are_ignored() {
        let provider = [
            w("a", 0.0, 100.0),
            w("very", 150.0, 400.0),
            w("big", 450.0, 700.0),
            w("test", 750.0, 1000.0),
        ];
        let a = map_provider_words("a big test", &provider, iv(0.0, 1000.0), &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(
            a.words,
            [
                w("a", 0.0, 100.0),
                w("big", 450.0, 700.0),
                w("test", 750.0, 1000.0)
            ]
        );
    }

    #[test]
    fn unmatched_leading_and_trailing_tokens() {
        let provider = [w("middle", 500.0, 900.0)];
        let span = iv(0.0, 2000.0);
        let a = map_provider_words("start middle end", &provider, span, &[iv(100.0, 1800.0)]);
        assert_eq!(a.words[1], w("middle", 500.0, 900.0));
        assert_eq!(a.words[0].start_ms, 100.0);
        assert_eq!(a.words[2].end_ms, 1800.0);
        check_invariants(&a.words, span);
    }

    #[test]
    fn nothing_matches_falls_back_to_estimate() {
        let provider = [w("bonjour", 0.0, 500.0), w("monde", 500.0, 1000.0)];
        let span = iv(0.0, 1000.0);
        let regions = [iv(50.0, 950.0)];
        let a = map_provider_words("hello world", &provider, span, &regions);
        assert_eq!(a.source, AlignmentSource::Estimated);
        assert_eq!(a.words, estimate("hello world", span, &regions));
        let b = map_provider_words("hello world", &[], span, &regions);
        assert_eq!(b.source, AlignmentSource::Estimated);
        assert_eq!(b.words, a.words);
    }

    #[test]
    fn caption_override_partially_matches() {
        // Spoken "S Q L databases are fast", shown "SQL databases are fast".
        let provider = [
            w("S", 0.0, 150.0),
            w("Q", 150.0, 300.0),
            w("L", 300.0, 450.0),
            w("databases", 500.0, 1100.0),
            w("are", 1100.0, 1250.0),
            w("fast", 1250.0, 1600.0),
        ];
        let a = map_provider_words("SQL databases are fast", &provider, iv(0.0, 1600.0), &[]);
        assert_eq!(a.words[0], w("SQL", 0.0, 450.0));
        assert_eq!(a.source, AlignmentSource::Provider);
    }

    #[test]
    fn hostile_provider_times() {
        let provider = [
            w("one", 500.0, 400.0),
            w("two", 100.0, 200.0),
            w("three", f64::NAN, 900.0),
            w("four", -50.0, 5000.0),
        ];
        let span = iv(0.0, 1000.0);
        let a = map_provider_words("one two three four", &provider, span, &[]);
        check_invariants(&a.words, span);
        assert_eq!(a.words.len(), 4);
    }

    #[test]
    fn repeated_words() {
        let provider = [
            w("no", 0.0, 200.0),
            w("no", 300.0, 500.0),
            w("yes", 600.0, 900.0),
        ];
        let a = map_provider_words("no, no, no, yes", &provider, iv(0.0, 1000.0), &[]);
        assert_eq!(a.words.len(), 4);
        assert_eq!(a.words[3], w("yes", 600.0, 900.0));
        check_invariants(&a.words, iv(0.0, 1000.0));
    }

    #[test]
    fn empty_text() {
        let a = map_provider_words("", &[w("x", 0.0, 1.0)], iv(0.0, 1.0), &[]);
        assert!(a.words.is_empty());
        assert_eq!(a.source, AlignmentSource::Provider);
    }

    #[test]
    fn greedy_alignment_for_long_inputs() {
        let a: Vec<u32> = (0..50).collect();
        let mut b: Vec<u32> = (0..50).collect();
        b.insert(10, 999);
        b.remove(30);
        let pairs = greedy_pairs(&a, &b);
        assert!(pairs.iter().all(|&(i, j)| a[i] == b[j]));
        assert!(pairs.len() >= 47, "{}", pairs.len());
        assert_eq!(lcs_pairs(&a, &b).len(), 49);
    }

    #[test]
    fn long_cue_uses_greedy_path_and_stays_valid() {
        let words: Vec<String> = (0..1200).map(|i| format!("w{i}")).collect();
        let text = words.join(" ");
        let provider: Vec<Word> = words
            .iter()
            .enumerate()
            .map(|(i, t)| w(t, i as f64 * 100.0, i as f64 * 100.0 + 90.0))
            .collect();
        let span = iv(0.0, 120_000.0);
        let a = map_provider_words(&text, &provider, span, &[]);
        assert_eq!(a.source, AlignmentSource::Provider);
        assert_eq!(a.words[600], w("w600", 60_000.0, 60_090.0));
    }
}
