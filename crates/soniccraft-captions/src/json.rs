//! JSON exports: `<stem>.words.json` (DESIGN §10.1) and the HyperFrames-compatible
//! `<stem>.transcript.json` (DESIGN §10.3).
//!
//! Field order follows the struct declarations (the documented order). Times are finite and
//! non-negative: words.json uses milliseconds rounded to 0.1 ms, transcript.json uses seconds rounded to
//! whole milliseconds. The `*_string` variants pretty-print with two-space indentation and a trailing
//! newline, byte-stable for golden files.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Caption, Word};
use crate::writers::round_ms;

/// Version of the words.json format.
pub const WORDS_JSON_VERSION: &str = "1.0";

/// The `<stem>.words.json` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordsDocument {
    /// Format version, [`WORDS_JSON_VERSION`].
    pub version: String,
    /// Captions in display order.
    pub captions: Vec<CaptionRecord>,
    /// Words in chronological order of their cues' start times, then word order.
    pub words: Vec<WordRecord>,
}

/// One caption in words.json (and the preview payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptionRecord {
    /// 1-based caption number.
    pub index: usize,
    /// Id of the caption's cue.
    pub cue_id: String,
    /// Id of the cue's track.
    pub track_id: String,
    /// Speaker display label, or `null`.
    pub speaker: Option<String>,
    /// Milliseconds from the start of the output, rounded to 0.1 ms.
    pub start_ms: f64,
    /// Milliseconds from the start of the output, rounded to 0.1 ms.
    pub end_ms: f64,
    /// The caption text on one line.
    pub text: String,
    /// The caption's line layout.
    pub lines: Vec<String>,
}

/// One word in words.json (and the preview payload).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordRecord {
    /// The word including attached punctuation.
    pub text: String,
    /// Milliseconds from the start of the output, rounded to 0.1 ms.
    pub start_ms: f64,
    /// Milliseconds from the start of the output, rounded to 0.1 ms.
    pub end_ms: f64,
    /// Id of the word's cue.
    pub cue_id: String,
    /// Id of the cue's track.
    pub track_id: String,
    /// Speaker display label, or `null`.
    pub speaker: Option<String>,
}

/// One word of the HyperFrames transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptWord {
    /// `w<index>`, 0-based in chronological order.
    pub id: String,
    /// The word including attached punctuation.
    pub text: String,
    /// Seconds from the start of the output, rounded to milliseconds.
    pub start: f64,
    /// Seconds from the start of the output, rounded to milliseconds.
    pub end: f64,
}

/// Build the words.json document from captions (as returned by [`crate::build_captions`]).
pub fn words_document(captions: &[Caption]) -> WordsDocument {
    let caption_records = captions
        .iter()
        .map(|c| CaptionRecord {
            index: c.index,
            cue_id: c.cue_id.clone(),
            track_id: c.track_id.clone(),
            speaker: c.speaker.clone(),
            start_ms: tenth_ms(c.start_ms),
            end_ms: tenth_ms(c.end_ms),
            text: c.text.clone(),
            lines: c.lines.clone(),
        })
        .collect();
    WordsDocument {
        version: WORDS_JSON_VERSION.to_string(),
        captions: caption_records,
        words: word_records(captions),
    }
}

/// The words.json document as a JSON value.
pub fn to_words_json(captions: &[Caption]) -> Value {
    to_value(&words_document(captions))
}

/// The words.json document, pretty-printed, with a trailing newline.
pub fn to_words_json_string(captions: &[Caption]) -> String {
    to_pretty(&words_document(captions))
}

/// The words of all captions in words.json order: cues by start time (ties: input order), words in
/// order within each cue.
pub fn word_records(captions: &[Caption]) -> Vec<WordRecord> {
    ordered_words(captions)
        .into_iter()
        .map(|(c, w)| WordRecord {
            text: w.text.clone(),
            start_ms: tenth_ms(w.start_ms),
            end_ms: tenth_ms(w.end_ms),
            cue_id: c.cue_id.clone(),
            track_id: c.track_id.clone(),
            speaker: c.speaker.clone(),
        })
        .collect()
}

/// The HyperFrames transcript: every word in chronological order (stable by start time over the
/// words.json order), times in seconds rounded from the exact word times.
pub fn transcript(captions: &[Caption]) -> Vec<TranscriptWord> {
    let mut words = ordered_words(captions);
    words.sort_by(|a, b| finite_or_zero(a.1.start_ms).total_cmp(&finite_or_zero(b.1.start_ms)));
    words
        .into_iter()
        .enumerate()
        .map(|(i, (_, w))| {
            let start = seconds(w.start_ms);
            TranscriptWord {
                id: format!("w{i}"),
                text: w.text.clone(),
                start,
                end: seconds(w.end_ms).max(start),
            }
        })
        .collect()
}

/// `(caption, word)` pairs in words.json order.
fn ordered_words(captions: &[Caption]) -> Vec<(&Caption, &Word)> {
    // cue_order → (cue start, captions in slice order)
    let mut cues: BTreeMap<usize, (f64, Vec<&Caption>)> = BTreeMap::new();
    for c in captions {
        cues.entry(c.cue_order)
            .or_insert_with(|| (finite_or_zero(c.cue_start_ms), Vec::new()))
            .1
            .push(c);
    }
    let mut ordered: Vec<(usize, f64, Vec<&Caption>)> =
        cues.into_iter().map(|(o, (s, cs))| (o, s, cs)).collect();
    ordered.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    ordered
        .into_iter()
        .flat_map(|(_, _, cue_captions)| cue_captions)
        .flat_map(|c| c.words.iter().map(move |w| (c, w)))
        .collect()
}

fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// The transcript as a JSON value (an array).
pub fn to_transcript_json(captions: &[Caption]) -> Value {
    to_value(&transcript(captions))
}

/// The transcript, pretty-printed, with a trailing newline.
pub fn to_transcript_json_string(captions: &[Caption]) -> String {
    to_pretty(&transcript(captions))
}

/// Milliseconds rounded to 0.1 ms; negative, `-0.0` and non-finite values become 0.
fn tenth_ms(ms: f64) -> f64 {
    if !ms.is_finite() || ms <= 0.0 {
        return 0.0;
    }
    let rounded = (ms * 10.0).round() / 10.0;
    if rounded.is_finite() { rounded } else { 0.0 }
}

/// Milliseconds → seconds rounded to whole milliseconds.
fn seconds(ms: f64) -> f64 {
    round_ms(ms) as f64 / 1000.0
}

fn to_value<T: Serialize>(value: &T) -> Value {
    // These types contain only strings, integers and finite floats, which always serialize.
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn to_pretty<T: Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caption(
        index: usize,
        cue: &str,
        order: usize,
        cue_start: f64,
        words: &[(&str, f64, f64)],
    ) -> Caption {
        Caption {
            index,
            cue_id: cue.into(),
            track_id: "voice".into(),
            speaker: (cue != "anon").then(|| "Narrator".to_string()),
            label: None,
            start_ms: words[0].1,
            end_ms: words[words.len() - 1].2,
            text: words.iter().map(|w| w.0).collect::<Vec<_>>().join(" "),
            lines: vec![words.iter().map(|w| w.0).collect::<Vec<_>>().join(" ")],
            words: words.iter().map(|&(t, s, e)| Word::new(t, s, e)).collect(),
            cue_start_ms: cue_start,
            cue_order: order,
        }
    }

    #[test]
    fn words_json_shape_matches_design() {
        let captions = [caption(
            1,
            "cue_01",
            0,
            0.0,
            &[("Autonomous", 0.0, 610.04), ("software", 610.04, 1020.06)],
        )];
        let value = to_words_json(&captions);
        assert_eq!(
            value,
            json!({
                "version": "1.0",
                "captions": [{
                    "index": 1, "cue_id": "cue_01", "track_id": "voice", "speaker": "Narrator",
                    "start_ms": 0.0, "end_ms": 1020.1, "text": "Autonomous software",
                    "lines": ["Autonomous software"]
                }],
                "words": [
                    {"text": "Autonomous", "start_ms": 0.0, "end_ms": 610.0, "cue_id": "cue_01", "track_id": "voice", "speaker": "Narrator"},
                    {"text": "software", "start_ms": 610.0, "end_ms": 1020.1, "cue_id": "cue_01", "track_id": "voice", "speaker": "Narrator"}
                ]
            })
        );
        // Key order is the documented order.
        let text = to_words_json_string(&captions);
        let caption_keys = [
            "\"index\"",
            "\"cue_id\"",
            "\"track_id\"",
            "\"speaker\"",
            "\"start_ms\"",
            "\"end_ms\"",
            "\"text\"",
            "\"lines\"",
        ];
        let positions: Vec<usize> = caption_keys
            .iter()
            .map(|k| text.find(k).expect("key present"))
            .collect();
        assert!(positions.windows(2).all(|p| p[0] < p[1]), "{text}");
        assert!(text.ends_with("}\n"));
    }

    #[test]
    fn null_speaker_and_rounding() {
        let captions = [caption(
            1,
            "anon",
            0,
            0.0,
            &[("x", -0.0, 0.04), ("y", 1.25, f64::NAN)],
        )];
        let doc = words_document(&captions);
        assert_eq!(doc.captions[0].speaker, None);
        assert_eq!(doc.words[0].start_ms.to_bits(), 0.0f64.to_bits());
        assert_eq!(doc.words[0].end_ms, 0.0);
        assert_eq!(doc.words[1].start_ms, 1.3);
        assert_eq!(doc.words[1].end_ms, 0.0);
        assert!(to_words_json_string(&captions).contains("\"speaker\": null"));
    }

    #[test]
    fn words_follow_cue_start_order() {
        // Cue B starts earlier than cue A but its first caption starts later (leading silence): words.json
        // orders by cue start, captions keep display order.
        let a = caption(1, "a", 0, 1000.0, &[("a1", 1100.0, 1500.0)]);
        let b = caption(2, "b", 1, 900.0, &[("b1", 1200.0, 1400.0)]);
        let a2 = caption(3, "a", 0, 1000.0, &[("a2", 1600.0, 1700.0)]);
        let records = word_records(&[a, b, a2]);
        let texts: Vec<&str> = records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["b1", "a1", "a2"]);
    }

    #[test]
    fn transcript_is_chronological_in_seconds() {
        let a = caption(
            1,
            "a",
            0,
            0.0,
            &[("Welcome", 120.4, 480.0), ("back.", 480.0, 910.6)],
        );
        let b = caption(2, "b", 1, 300.0, &[("Hi", 300.0, 400.0)]);
        let value = to_transcript_json(&[a, b]);
        assert_eq!(
            value,
            json!([
                {"id": "w0", "text": "Welcome", "start": 0.12, "end": 0.48},
                {"id": "w1", "text": "Hi", "start": 0.3, "end": 0.4},
                {"id": "w2", "text": "back.", "start": 0.48, "end": 0.911}
            ])
        );
        assert_eq!(to_transcript_json_string(&[]), "[]\n");
    }

    #[test]
    fn transcript_rounds_from_exact_times() {
        // 1234.46 ms is 1234.5 in words.json but 1.234 s (not 1.235) in the transcript.
        let c = caption(1, "a", 0, 0.0, &[("x", 1234.46, 1300.0)]);
        assert_eq!(
            words_document(std::slice::from_ref(&c)).words[0].start_ms,
            1234.5
        );
        assert_eq!(transcript(&[c])[0].start, 1.234);
    }
}
