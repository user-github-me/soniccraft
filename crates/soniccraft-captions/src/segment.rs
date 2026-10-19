//! Caption segmentation and line breaking (DESIGN §6).
//!
//! Per cue (cues are never merged), words are added greedily to the current caption until the next word
//! would not fit into `max_lines` lines of `max_chars_per_line` (with real line breaking) or would make
//! the caption longer than `max_duration_ms`. Once a caption is at least half full, it ends after
//! sentence-ending punctuation. When a caption overflows, it ends after clause punctuation (`;` `:`
//! dashes), else after the last comma, provided that keeps it at least half full (otherwise right before
//! the word that did not fit).
//!
//! Lines are balanced: the break points minimize the deviation of line lengths from their mean, with a
//! bonus for breaking after punctuation (≈ ⅛–⅕ of a line); ties go to the shorter first line. A word
//! longer than a line gets a line of its own.
//!
//! Captions start at their first word and end at their last word, then are extended toward
//! `min_duration_ms` without overlapping the next caption of the same track. They are sorted by start
//! time (ties: input order of the cues, then caption order) and numbered from 1. Captions of different
//! tracks may overlap.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::align::estimate;
use crate::model::{Caption, CaptionConfig, CueInput, Interval, Word};
use crate::text::{BreakStrength, ZWSP, break_strength, display_len, normalize_whitespace};

/// Build the captions of all `cues` (see the module docs).
///
/// Cues with empty display text are skipped. Words with empty text are ignored; non-finite word times
/// are repaired from their neighbours, and word times are made non-decreasing and non-overlapping. With
/// [`CaptionConfig::timeline_end_ms`] set, words starting at or after it are dropped (content cut from
/// the output gets no captions) and word ends are clamped to it.
pub fn build_captions(cues: &[CueInput], config: &CaptionConfig) -> Vec<Caption> {
    let cfg = Settings::from(config);
    let mut captions = Vec::new();
    for (order, cue) in cues.iter().enumerate() {
        let text = normalize_whitespace(&cue.text);
        if text.is_empty() {
            continue;
        }
        let words = prepare_words(cue, &text, cfg.timeline_end);
        if words.is_empty() {
            continue;
        }
        let units = layout_units(&text, &words);
        let speaker = clean_label(cue.speaker.as_deref());
        let label = if config.speaker_labels {
            speaker.clone()
        } else {
            None
        };
        let cue_start_ms = if cue.start_ms.is_finite() {
            cue.start_ms
        } else {
            words[0].start_ms
        };
        for range in segment(&units, &cfg) {
            let group = &units[range.clone()];
            captions.push(Caption {
                index: 0,
                cue_id: cue.cue_id.clone(),
                track_id: cue.track_id.clone(),
                speaker: speaker.clone(),
                label: label.clone(),
                start_ms: group[0].start,
                end_ms: group[group.len() - 1].end,
                text: join_units(group),
                lines: break_lines(group, &cfg),
                words: words[range].to_vec(),
                cue_start_ms,
                cue_order: order,
            });
        }
    }
    // Stable sort: captions of one cue keep their order on equal keys.
    captions.sort_by(|a, b| {
        a.start_ms
            .total_cmp(&b.start_ms)
            .then(a.cue_order.cmp(&b.cue_order))
    });
    extend_min_duration(&mut captions, &cfg);
    for (i, caption) in captions.iter_mut().enumerate() {
        caption.index = i + 1;
    }
    captions
}

/// The DSL's `speaker_labels: auto`: true when the cues carry at least two distinct speaker labels.
pub fn auto_speaker_labels(cues: &[CueInput]) -> bool {
    let mut labels: Vec<String> = cues
        .iter()
        .filter_map(|c| clean_label(c.speaker.as_deref()))
        .collect();
    labels.sort();
    labels.dedup();
    labels.len() >= 2
}

/// Sanitized layout settings.
#[derive(Debug, Clone, Copy)]
struct Settings {
    max_chars: usize,
    max_lines: usize,
    max_duration: f64,
    min_duration: f64,
    timeline_end: Option<f64>,
}

impl From<&CaptionConfig> for Settings {
    fn from(c: &CaptionConfig) -> Self {
        let max_duration = if c.max_duration_ms.is_finite() && c.max_duration_ms > 0.0 {
            c.max_duration_ms
        } else {
            f64::INFINITY
        };
        let min_duration = if c.min_duration_ms.is_finite() && c.min_duration_ms > 0.0 {
            c.min_duration_ms
        } else {
            0.0
        };
        Settings {
            max_chars: c.max_chars_per_line.max(1),
            max_lines: c.max_lines.max(1),
            max_duration,
            min_duration: min_duration.min(max_duration),
            timeline_end: c.timeline_end_ms.filter(|t| t.is_finite()),
        }
    }
}

impl Settings {
    fn capacity(&self) -> usize {
        self.max_chars.saturating_mul(self.max_lines)
    }
}

fn clean_label(label: Option<&str>) -> Option<String> {
    label.map(normalize_whitespace).filter(|l| !l.is_empty())
}

/// The cue's words with sanitized times (an estimate over the cue span when none were given), cut at
/// the timeline end.
fn prepare_words(cue: &CueInput, text: &str, timeline_end: Option<f64>) -> Vec<Word> {
    let mut words = sanitized_words(cue, text);
    if let Some(end) = timeline_end {
        words.retain(|w| w.start_ms < end);
        for w in &mut words {
            w.end_ms = w.end_ms.min(end).max(w.start_ms);
        }
    }
    words
}

fn sanitized_words(cue: &CueInput, text: &str) -> Vec<Word> {
    let span = Interval::new(cue.start_ms, cue.end_ms).sanitized();
    let given: Vec<Word> = cue
        .words
        .iter()
        .map(|w| Word::new(normalize_whitespace(&w.text), w.start_ms, w.end_ms))
        .filter(|w| !w.text.is_empty())
        .collect();
    if given.is_empty() {
        return estimate(text, span, &[]);
    }
    let mut words = given;
    let mut prev_end = span.start_ms.max(0.0);
    for w in &mut words {
        let start = if w.start_ms.is_finite() {
            w.start_ms
        } else {
            prev_end
        };
        let end = if w.end_ms.is_finite() {
            w.end_ms
        } else {
            start
        };
        w.start_ms = start.max(0.0);
        w.end_ms = end.max(w.start_ms);
        prev_end = w.end_ms;
    }
    let mut prev_start = 0.0f64;
    for w in &mut words {
        w.start_ms = w.start_ms.max(prev_start);
        w.end_ms = w.end_ms.max(w.start_ms);
        prev_start = w.start_ms;
    }
    for k in 1..words.len() {
        let next_start = words[k].start_ms;
        let cur = &mut words[k - 1];
        if cur.end_ms > next_start {
            cur.end_ms = next_start.max(cur.start_ms);
        }
    }
    words
}

/// A word prepared for layout.
#[derive(Debug, Clone)]
struct Unit {
    text: String,
    /// A space separates this unit from the previous one on the same line.
    space_before: bool,
    /// Display length in grapheme clusters.
    len: usize,
    break_after: BreakStrength,
    start: f64,
    end: f64,
}

impl Unit {
    fn sep_len(&self) -> usize {
        usize::from(self.space_before)
    }
}

/// Recover the separators between words by locating them in the display text (whitespace-insensitive,
/// zero-width spaces count as joints). Words not found in the text are separated by a space.
fn layout_units(text: &str, words: &[Word]) -> Vec<Unit> {
    let haystack: String = text.chars().filter(|&c| c != ZWSP).collect();
    let mut cursor = 0;
    let mut units: Vec<Unit> = Vec::with_capacity(words.len());
    for (k, w) in words.iter().enumerate() {
        let space_before = match haystack[cursor..].find(w.text.as_str()) {
            Some(offset) => {
                let skipped = &haystack[cursor..cursor + offset];
                cursor += offset + w.text.len();
                k > 0 && !skipped.is_empty()
            }
            None => k > 0,
        };
        let next = words.get(k + 1).map(|n| n.text.as_str());
        units.push(Unit {
            text: w.text.clone(),
            space_before,
            len: display_len(&w.text),
            break_after: break_strength(&w.text, next),
            start: w.start_ms,
            end: w.end_ms,
        });
    }
    units
}

/// Length of `units` on one line.
fn joined_len(units: &[Unit]) -> usize {
    units
        .iter()
        .enumerate()
        .map(|(i, u)| u.len + if i > 0 { u.sep_len() } else { 0 })
        .sum()
}

fn join_units(units: &[Unit]) -> String {
    let mut out = String::new();
    for (i, u) in units.iter().enumerate() {
        if i > 0 && u.space_before {
            out.push(' ');
        }
        out.push_str(&u.text);
    }
    out
}

/// Minimum number of lines for `units` with lines of at most `max_chars` (first fit; a unit longer than a
/// line occupies a line of its own).
fn line_count(units: &[Unit], max_chars: usize) -> usize {
    let mut lines = 0;
    let mut current: Option<usize> = None;
    for u in units {
        current = match current {
            Some(len) if len + u.sep_len() + u.len <= max_chars => Some(len + u.sep_len() + u.len),
            _ => {
                lines += 1;
                Some(u.len)
            }
        };
    }
    lines
}

/// Split one cue's units into caption ranges.
fn segment(units: &[Unit], cfg: &Settings) -> Vec<Range<usize>> {
    let half = cfg.capacity() as f64 / 2.0;
    let mut ranges = Vec::new();
    let mut start = 0;
    for k in 0..units.len() {
        while start < k {
            let current = &units[start..k];
            if current[current.len() - 1].break_after == BreakStrength::Sentence
                && joined_len(current) as f64 >= half
            {
                ranges.push(start..k);
                start = k;
                break;
            }
            let fits = line_count(&units[start..=k], cfg.max_chars) <= cfg.max_lines;
            let short_enough = units[k].end - units[start].start <= cfg.max_duration;
            if fits && short_enough {
                break;
            }
            // Overflow: end the caption after the strongest (then latest) punctuation that keeps it at least
            // half full, else right before the word that did not fit.
            let split = (start + 1..k)
                .filter(|&s| {
                    units[s - 1].break_after >= BreakStrength::Comma
                        && joined_len(&units[start..s]) as f64 >= half
                })
                .max_by_key(|&s| (units[s - 1].break_after, s))
                .unwrap_or(k);
            ranges.push(start..split);
            start = split;
        }
    }
    if start < units.len() {
        ranges.push(start..units.len());
    }
    ranges
}

/// Bonus (in characters of line-length deviation) for breaking a line after punctuation.
fn break_bonus(strength: BreakStrength, max_chars: usize) -> f64 {
    let factor = match strength {
        BreakStrength::Sentence => 0.4,
        BreakStrength::Clause => 0.3,
        BreakStrength::Comma => 0.25,
        BreakStrength::None => 0.0,
    };
    factor * max_chars as f64
}

/// Balanced line layout of one caption (see the module docs).
fn break_lines(units: &[Unit], cfg: &Settings) -> Vec<String> {
    let lines = line_count(units, cfg.max_chars);
    let n = units.len();
    if lines <= 1 || n < 2 {
        return vec![join_units(units)];
    }
    // Prefix lengths so the length of units[s..e] on one line is O(1).
    let mut prefix = vec![0usize; n + 1];
    for (i, u) in units.iter().enumerate() {
        prefix[i + 1] = prefix[i] + u.len + u.sep_len();
    }
    let line_len = |s: usize, e: usize| prefix[e] - prefix[s] - units[s].sep_len();
    let target = (joined_len(units) as f64 - (lines - 1) as f64) / lines as f64;
    // best[l][e]: minimal cost of laying units[..e] out in l lines; from[l][e]: start of the last line.
    let mut best = vec![vec![f64::INFINITY; n + 1]; lines + 1];
    let mut from = vec![vec![0usize; n + 1]; lines + 1];
    best[0][0] = 0.0;
    for l in 1..=lines {
        for e in l..=n {
            for s in (l - 1)..e {
                if !best[l - 1][s].is_finite() {
                    continue;
                }
                let len = line_len(s, e);
                if len > cfg.max_chars && e - s > 1 {
                    continue;
                }
                let bonus = if s > 0 {
                    break_bonus(units[s - 1].break_after, cfg.max_chars)
                } else {
                    0.0
                };
                let cost = best[l - 1][s] + (len as f64 - target).abs() - bonus;
                if cost < best[l][e] {
                    best[l][e] = cost;
                    from[l][e] = s;
                }
            }
        }
    }
    if !best[lines][n].is_finite() {
        return vec![join_units(units)];
    }
    let mut bounds = Vec::with_capacity(lines);
    let mut e = n;
    for l in (1..=lines).rev() {
        let s = from[l][e];
        bounds.push(s..e);
        e = s;
    }
    bounds.reverse();
    bounds.into_iter().map(|r| join_units(&units[r])).collect()
}

/// Extend captions shorter than the minimum duration (DESIGN §6) without overlapping the next caption
/// of the same track; also make every caption at least 1 ms long so writers never emit empty cues.
fn extend_min_duration(captions: &mut [Caption], cfg: &Settings) {
    let mut next_start: BTreeMap<String, f64> = BTreeMap::new();
    for caption in captions.iter_mut().rev() {
        let mut limit = next_start
            .get(&caption.track_id)
            .copied()
            .unwrap_or(f64::INFINITY);
        if let Some(end) = cfg.timeline_end {
            limit = limit.min(end);
        }
        let target = caption.start_ms + cfg.min_duration;
        if caption.end_ms < target {
            caption.end_ms = caption.end_ms.max(target.min(limit));
        }
        if caption.end_ms - caption.start_ms < 1.0 {
            caption.end_ms = caption.start_ms + 1.0;
        }
        next_start.insert(caption.track_id.clone(), caption.start_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CaptionConfig {
        CaptionConfig::default()
    }

    fn cue(id: &str, track: &str, text: &str, start: f64, end: f64) -> CueInput {
        CueInput {
            cue_id: id.into(),
            track_id: track.into(),
            speaker: None,
            text: text.into(),
            start_ms: start,
            end_ms: end,
            words: Vec::new(),
        }
    }

    /// Words of `text` spaced `step` ms apart starting at `start`.
    fn timed(text: &str, start: f64, step: f64) -> Vec<Word> {
        crate::text::tokenize(text)
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                Word::new(
                    t.text,
                    start + i as f64 * step,
                    start + (i + 1) as f64 * step,
                )
            })
            .collect()
    }

    fn texts(captions: &[Caption]) -> Vec<String> {
        captions.iter().map(|c| c.text.clone()).collect()
    }

    #[test]
    fn short_cue_is_one_caption() {
        let captions = build_captions(&[cue("c1", "t", "Hello world.", 0.0, 1000.0)], &cfg());
        assert_eq!(captions.len(), 1);
        let c = &captions[0];
        assert_eq!(
            (c.index, c.cue_id.as_str(), c.text.as_str()),
            (1, "c1", "Hello world.")
        );
        assert_eq!(c.lines, ["Hello world."]);
        assert_eq!((c.start_ms, c.end_ms), (0.0, 1000.0));
        assert_eq!(c.words.len(), 2);
    }

    #[test]
    fn capacity_and_balanced_lines() {
        let text = "Autonomous software development is changing how teams build, test, and ship products every single day.";
        let mut c = cue("c1", "t", text, 0.0, 5000.0);
        c.words = timed(text, 0.0, 250.0);
        let captions = build_captions(&[c], &cfg());
        // "products" overflows two lines; the caption ends after the last comma that keeps it half full.
        assert_eq!(
            texts(&captions),
            [
                "Autonomous software development is changing how teams build, test,",
                "and ship products every single day."
            ]
        );
        assert_eq!(
            captions[0].lines,
            [
                "Autonomous software development",
                "is changing how teams build, test,"
            ]
        );
        assert_eq!(captions[1].lines, ["and ship products every single day."]);
        for caption in &captions {
            assert!(caption.lines.len() <= 2);
            assert!(caption.lines.iter().all(|l| display_len(l) <= 42));
        }
        // Captions cover their words exactly.
        assert_eq!(captions[0].start_ms, 0.0);
        assert_eq!(
            captions[0].end_ms,
            captions[0].words.last().map_or(0.0, |w| w.end_ms)
        );
        assert_eq!(captions[1].start_ms, captions[1].words[0].start_ms);
    }

    #[test]
    fn prefers_sentence_end_once_half_full() {
        let text = "This first sentence is long enough to fill half. Second one. Third sentence follows here.";
        let mut c = cue("c1", "t", text, 0.0, 9000.0);
        c.words = timed(text, 0.0, 300.0);
        let captions = build_captions(&[c], &cfg());
        assert_eq!(
            captions[0].text,
            "This first sentence is long enough to fill half."
        );
        // "Second one." is short (< half) so the next sentence joins it.
        assert_eq!(captions[1].text, "Second one. Third sentence follows here.");
    }

    #[test]
    fn overflow_prefers_clause_punctuation() {
        let text = "The committee reviewed every single proposal; the results, as expected, were \
                    consistent across all runs.";
        let mut c = cue("c1", "t", text, 0.0, 9000.0);
        c.words = timed(text, 0.0, 300.0);
        let captions = build_captions(&[c], &cfg());
        // "proposal;" (45 chars), "results," and "expected," all keep the caption half full; the clause
        // break wins over the later commas.
        assert_eq!(
            captions[0].text,
            "The committee reviewed every single proposal;"
        );
        let mut config = cfg();
        config.max_lines = 1;
        config.max_chars_per_line = 60;
        let mut c = cue("c1", "t", text, 0.0, 9000.0);
        c.words = timed(text, 0.0, 300.0);
        let captions = build_captions(&[c], &config);
        assert_eq!(
            captions[0].text,
            "The committee reviewed every single proposal;"
        );
        // Below half full, punctuation is not used: "twice;" is only 29 of 84 characters.
        let text = "We measured everything twice; the results, as expected, were consistent across all runs.";
        let mut c = cue("c1", "t", text, 0.0, 9000.0);
        c.words = timed(text, 0.0, 300.0);
        let captions = build_captions(&[c], &cfg());
        assert_eq!(
            captions[0].text,
            "We measured everything twice; the results, as expected,"
        );
    }

    #[test]
    fn timeline_end_cuts_words() {
        let mut config = cfg();
        config.timeline_end_ms = Some(1500.0);
        let mut c = cue("c", "t", "one two three four", 0.0, 4000.0);
        c.words = timed("one two three four", 0.0, 1000.0);
        let captions = build_captions(&[c], &config);
        assert_eq!(texts(&captions), ["one two"]);
        assert_eq!(captions[0].end_ms, 1500.0);
        let late = cue("late", "t", "Too late.", 2000.0, 3000.0);
        assert!(build_captions(&[late], &config).is_empty());
    }

    #[test]
    fn max_duration_splits() {
        let text = "one two three four five six seven eight";
        let mut c = cue("c1", "t", text, 0.0, 8000.0);
        c.words = timed(text, 0.0, 1000.0);
        let captions = build_captions(&[c], &cfg());
        assert_eq!(
            texts(&captions),
            ["one two three four five six", "seven eight"]
        );
        assert!(captions.iter().all(|c| c.duration_ms() <= 6000.0));
    }

    #[test]
    fn overlong_word_gets_its_own_line() {
        let long = "Pneumonoultramicroscopicsilicovolcanoconiosis-related";
        let text = format!("A {long} case");
        let captions = build_captions(&[cue("c1", "t", &text, 0.0, 3000.0)], &cfg());
        // "A" and the long word fill both lines; "case" no longer fits.
        assert_eq!(texts(&captions), [format!("A {long}"), "case".to_string()]);
        assert_eq!(captions[0].lines, ["A", long]);
        let mut config = cfg();
        config.max_lines = 1;
        let captions = build_captions(&[cue("c1", "t", &text, 0.0, 3000.0)], &config);
        assert_eq!(texts(&captions), ["A", long, "case"]);
    }

    #[test]
    fn three_lines() {
        let mut config = cfg();
        config.max_lines = 3;
        config.max_chars_per_line = 20;
        let text = "one two three four five six seven eight nine ten eleven";
        let captions = build_captions(&[cue("c1", "t", text, 0.0, 4000.0)], &config);
        assert_eq!(captions.len(), 1);
        assert_eq!(captions[0].lines.len(), 3);
        assert!(captions[0].lines.iter().all(|l| display_len(l) <= 20));
        assert_eq!(captions[0].lines.join(" "), text);
    }

    #[test]
    fn min_duration_extension_respects_same_track_only() {
        let mut a = cue("a", "t1", "Hi.", 0.0, 300.0);
        a.words = vec![Word::new("Hi.", 0.0, 300.0)];
        let mut b = cue("b", "t1", "Yes.", 500.0, 700.0);
        b.words = vec![Word::new("Yes.", 500.0, 700.0)];
        let mut other = cue("x", "t2", "Overlap.", 100.0, 200.0);
        other.words = vec![Word::new("Overlap.", 100.0, 200.0)];
        let captions = build_captions(&[a, b, other], &cfg());
        let by_id = |id: &str| {
            captions
                .iter()
                .find(|c| c.cue_id == id)
                .cloned()
                .expect("caption exists")
        };
        assert_eq!(by_id("a").end_ms, 500.0); // blocked by "b" on the same track
        assert_eq!(by_id("b").end_ms, 1300.0); // free to extend
        assert_eq!(by_id("x").end_ms, 900.0); // other track: may overlap "a"
        assert_eq!(texts(&captions), ["Hi.", "Overlap.", "Yes."]);
        assert_eq!(
            captions.iter().map(|c| c.index).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn min_duration_respects_timeline_end_and_max_duration() {
        let mut config = cfg();
        config.timeline_end_ms = Some(1000.0);
        let mut a = cue("a", "t", "Bye.", 600.0, 700.0);
        a.words = vec![Word::new("Bye.", 600.0, 700.0)];
        assert_eq!(build_captions(&[a.clone()], &config)[0].end_ms, 1000.0);
        config.timeline_end_ms = None;
        config.min_duration_ms = 10_000.0;
        config.max_duration_ms = 2000.0;
        assert_eq!(build_captions(&[a], &config)[0].end_ms, 2600.0);
    }

    #[test]
    fn ties_sort_by_cue_order() {
        let captions = build_captions(
            &[
                cue("first", "t1", "One.", 1000.0, 2000.0),
                cue("second", "t2", "Two.", 1000.0, 2000.0),
            ],
            &cfg(),
        );
        assert_eq!(
            captions
                .iter()
                .map(|c| c.cue_id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        let captions = build_captions(
            &[
                cue("second", "t2", "Two.", 1000.0, 2000.0),
                cue("first", "t1", "One.", 1000.0, 2000.0),
            ],
            &cfg(),
        );
        assert_eq!(
            captions
                .iter()
                .map(|c| c.cue_id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
    }

    #[test]
    fn empty_cues_are_skipped_and_cues_never_merge() {
        let captions = build_captions(
            &[
                cue("a", "t", "  ", 0.0, 100.0),
                cue("b", "t", "Short.", 0.0, 500.0),
                cue("c", "t", "Also short.", 600.0, 900.0),
            ],
            &cfg(),
        );
        assert_eq!(texts(&captions), ["Short.", "Also short."]);
    }

    #[test]
    fn cjk_lines_break_without_spaces() {
        let mut config = cfg();
        config.max_chars_per_line = 8;
        let text = "你好，世界。今天我们讨论人工智能的未来。";
        let captions = build_captions(&[cue("c", "t", text, 0.0, 6000.0)], &config);
        let joined: String = captions.iter().map(|c| c.text.clone()).collect();
        assert_eq!(joined, text);
        for caption in &captions {
            assert!(!caption.text.contains(' '));
            assert!(
                caption.lines.iter().all(|l| display_len(l) <= 8),
                "{:?}",
                caption.lines
            );
        }
    }

    #[test]
    fn separators_are_recovered_from_text() {
        let text = "wait—what? 10\u{a0}km";
        let words = vec![
            Word::new("wait—", 0.0, 100.0),
            Word::new("what?", 100.0, 200.0),
            Word::new("10\u{a0}km", 300.0, 400.0),
        ];
        let mut c = cue("c", "t", text, 0.0, 400.0);
        c.words = words;
        let captions = build_captions(&[c], &cfg());
        assert_eq!(captions[0].text, text);
        // Unknown words fall back to spaces.
        let mut c = cue("c", "t", "Hello world", 0.0, 400.0);
        c.words = vec![
            Word::new("hello", 0.0, 100.0),
            Word::new("WORLD", 100.0, 200.0),
        ];
        assert_eq!(build_captions(&[c], &cfg())[0].text, "hello WORLD");
    }

    #[test]
    fn bad_word_times_are_repaired() {
        let mut c = cue("c", "t", "a b c", 1000.0, 2000.0);
        c.words = vec![
            Word::new("a", f64::NAN, 1200.0),
            Word::new("b", 1100.0, 1050.0),
            Word::new("c", 900.0, f64::INFINITY),
        ];
        let captions = build_captions(&[c], &cfg());
        let words = &captions[0].words;
        for pair in words.windows(2) {
            assert!(pair[1].start_ms >= pair[0].end_ms);
        }
        assert!(
            words
                .iter()
                .all(|w| w.start_ms.is_finite() && w.end_ms.is_finite())
        );
    }

    #[test]
    fn labels_follow_config() {
        let mut c = cue("c", "t", "Hi.", 0.0, 1000.0);
        c.speaker = Some("  Dr.  Ada ".into());
        let mut config = cfg();
        let captions = build_captions(std::slice::from_ref(&c), &config);
        assert_eq!(captions[0].speaker.as_deref(), Some("Dr. Ada"));
        assert_eq!(captions[0].label, None);
        config.speaker_labels = true;
        let captions = build_captions(&[c], &config);
        assert_eq!(captions[0].label.as_deref(), Some("Dr. Ada"));
    }

    #[test]
    fn auto_labels() {
        let mut a = cue("a", "t", "x", 0.0, 1.0);
        let mut b = cue("b", "t", "y", 0.0, 1.0);
        assert!(!auto_speaker_labels(&[a.clone(), b.clone()]));
        a.speaker = Some("A".into());
        b.speaker = Some("A".into());
        assert!(!auto_speaker_labels(&[a.clone(), b.clone()]));
        b.speaker = Some("B".into());
        assert!(auto_speaker_labels(&[a, b]));
    }

    #[test]
    fn degenerate_config_values() {
        let config = CaptionConfig {
            max_chars_per_line: 0,
            max_lines: 0,
            max_duration_ms: f64::NAN,
            min_duration_ms: -5.0,
            speaker_labels: false,
            timeline_end_ms: Some(f64::NAN),
        };
        let captions = build_captions(&[cue("c", "t", "a b", 0.0, 100.0)], &config);
        assert_eq!(texts(&captions), ["a", "b"]);
        assert!(captions.iter().all(|c| c.end_ms > c.start_ms));
    }

    #[test]
    fn zero_length_cue_gets_one_millisecond() {
        let mut config = cfg();
        config.min_duration_ms = 0.0;
        let captions = build_captions(&[cue("c", "t", "Blink", 500.0, 500.0)], &config);
        assert_eq!((captions[0].start_ms, captions[0].end_ms), (500.0, 501.0));
    }
}
