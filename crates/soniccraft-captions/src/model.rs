//! Shared data types: timed words, time intervals, the per-cue input of [`crate::build_captions`], caption
//! layout settings and the resulting [`Caption`]s.
//!
//! All times are milliseconds as `f64`. Producers choose the time base (cue-relative or absolute); the
//! functions of this crate never mix bases, they only require that the arguments of one call share one.

use serde::{Deserialize, Serialize};

/// A word (display token) with a time span.
///
/// `text` includes attached punctuation (`"Hello,"`, `"world!"`, `"“Quote”"`). In alignment results the
/// times use the time base of the call's arguments; in [`CueInput::words`] and [`Caption::words`] they are
/// absolute (milliseconds from the start of the output file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// Display text of the word, including attached punctuation.
    pub text: String,
    /// Start time in milliseconds.
    pub start_ms: f64,
    /// End time in milliseconds (`>= start_ms`).
    pub end_ms: f64,
}

impl Word {
    /// Create a word.
    pub fn new(text: impl Into<String>, start_ms: f64, end_ms: f64) -> Self {
        Word {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    /// `end_ms - start_ms`, never negative.
    pub fn duration_ms(&self) -> f64 {
        (self.end_ms - self.start_ms).max(0.0)
    }

    /// A copy of this word moved by `offset_ms` (e.g. cue-relative → absolute, or compensating a silence
    /// trim).
    pub fn shifted(&self, offset_ms: f64) -> Word {
        Word {
            text: self.text.clone(),
            start_ms: self.start_ms + offset_ms,
            end_ms: self.end_ms + offset_ms,
        }
    }
}

/// A half-open time interval `[start_ms, end_ms)`, e.g. a cue's audio span or a detected speech region.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    /// Start in milliseconds.
    pub start_ms: f64,
    /// End in milliseconds.
    pub end_ms: f64,
}

impl Interval {
    /// Create an interval. No validation happens here; functions taking intervals sanitize them.
    pub const fn new(start_ms: f64, end_ms: f64) -> Self {
        Interval { start_ms, end_ms }
    }

    /// `end_ms - start_ms`, never negative.
    pub fn duration_ms(&self) -> f64 {
        (self.end_ms - self.start_ms).max(0.0)
    }

    /// Finite bounds with `end >= start`: a non-finite start becomes 0, a non-finite or earlier end becomes
    /// the start.
    pub(crate) fn sanitized(self) -> Interval {
        let start = if self.start_ms.is_finite() {
            self.start_ms
        } else {
            0.0
        };
        let end = if self.end_ms.is_finite() {
            self.end_ms.max(start)
        } else {
            start
        };
        Interval {
            start_ms: start,
            end_ms: end,
        }
    }
}

impl From<(f64, f64)> for Interval {
    /// `(start_ms, end_ms)` → `Interval`.
    fn from((start_ms, end_ms): (f64, f64)) -> Self {
        Interval::new(start_ms, end_ms)
    }
}

/// One speech cue to caption (input of [`crate::build_captions`]).
///
/// Pass cues in **declaration order** (track by track, then cue by cue, as `Program::cues()` yields
/// them): that order breaks ties when captions start at the same time. Cues excluded from subtitles
/// (`subtitles: false`) must not be passed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CueInput {
    /// Cue id (explicit or generated, e.g. `voice.cue1`).
    pub cue_id: String,
    /// Id of the cue's track.
    pub track_id: String,
    /// Speaker display label, if any. Exported as `speaker` in the JSON formats and rendered as a voice
    /// label in WebVTT/SRT when [`CaptionConfig::speaker_labels`] is on.
    pub speaker: Option<String>,
    /// Display text (the cue's `caption` override, else its `text`; see [`crate::text::display_text`]).
    /// Whitespace is normalized; a cue whose text is empty after normalization produces no captions.
    pub text: String,
    /// Absolute start of the cue's (trimmed) audio.
    pub start_ms: f64,
    /// Absolute end of the cue's (trimmed) audio.
    pub end_ms: f64,
    /// Timed words aligned to `text` (absolute times), normally produced by
    /// [`crate::align::map_provider_words`] or [`crate::align::estimate`] and shifted to the cue start.
    /// When empty, words are estimated over `[start_ms, end_ms]` from the text alone.
    #[serde(default)]
    pub words: Vec<Word>,
}

/// Caption layout settings (DESIGN §2.4 `subtitles`), resolved by the caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptionConfig {
    /// Maximum characters (grapheme clusters) per line. A single word longer than this gets a line of its
    /// own. Values below 1 are treated as 1.
    pub max_chars_per_line: usize,
    /// Maximum lines per caption. Values below 1 are treated as 1.
    pub max_lines: usize,
    /// A caption never spans longer than this (unless it consists of a single longer word). Non-positive
    /// or non-finite values mean "no limit".
    pub max_duration_ms: f64,
    /// Captions shorter than this are extended (at their end, never overlapping the next caption of the
    /// same track, never beyond `max_duration_ms` or `timeline_end_ms`).
    pub min_duration_ms: f64,
    /// Render speaker labels (WebVTT `<v Label>`, SRT `Label: `). The DSL's `auto` must be resolved by the
    /// caller (see [`crate::auto_speaker_labels`]).
    pub speaker_labels: bool,
    /// Total length of the output, if known: the minimum-duration extension never goes past it.
    #[serde(default)]
    pub timeline_end_ms: Option<f64>,
}

impl Default for CaptionConfig {
    /// The DESIGN §2.4 defaults: 42 characters × 2 lines, 6000 ms max, 800 ms min, labels off.
    fn default() -> Self {
        CaptionConfig {
            max_chars_per_line: 42,
            max_lines: 2,
            max_duration_ms: 6000.0,
            min_duration_ms: 800.0,
            speaker_labels: false,
            timeline_end_ms: None,
        }
    }
}

/// One caption (a subtitle cue), as produced by [`crate::build_captions`].
#[derive(Debug, Clone, PartialEq)]
pub struct Caption {
    /// 1-based position in the sorted caption list.
    pub index: usize,
    /// Id of the cue this caption belongs to (captions never span cues).
    pub cue_id: String,
    /// Id of the cue's track.
    pub track_id: String,
    /// Speaker display label of the cue (data; exported in JSON regardless of `speaker_labels`).
    pub speaker: Option<String>,
    /// Label rendered by the WebVTT/SRT writers: `Some` only when speaker labels are enabled and the cue
    /// has a speaker.
    pub label: Option<String>,
    /// Absolute start (= first word start).
    pub start_ms: f64,
    /// Absolute end (= last word end, possibly extended toward the minimum duration).
    pub end_ms: f64,
    /// The caption text on one line (words joined with their original separators).
    pub text: String,
    /// The balanced line layout of `text` (at most `max_lines` entries).
    pub lines: Vec<String>,
    /// The caption's words with absolute times.
    pub words: Vec<Word>,
    /// Absolute start of the caption's cue (orders the JSON word export, DESIGN §10.1).
    pub cue_start_ms: f64,
    /// Position of the caption's cue in the input of [`crate::build_captions`] (tie-breaker).
    pub cue_order: usize,
}

impl Caption {
    /// `end_ms - start_ms`, never negative.
    pub fn duration_ms(&self) -> f64 {
        (self.end_ms - self.start_ms).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_sanitize() {
        assert_eq!(
            Interval::new(10.0, 5.0).sanitized(),
            Interval::new(10.0, 10.0)
        );
        assert_eq!(
            Interval::new(f64::NAN, 5.0).sanitized(),
            Interval::new(0.0, 5.0)
        );
        assert_eq!(
            Interval::new(3.0, f64::INFINITY).sanitized(),
            Interval::new(3.0, 3.0)
        );
        assert_eq!(Interval::new(3.0, 7.0).duration_ms(), 4.0);
        assert_eq!(Interval::new(7.0, 3.0).duration_ms(), 0.0);
        assert_eq!(Interval::from((1.0, 2.0)), Interval::new(1.0, 2.0));
    }

    #[test]
    fn word_helpers() {
        let w = Word::new("hi", 100.0, 250.0);
        assert_eq!(w.duration_ms(), 150.0);
        assert_eq!(w.shifted(1000.0), Word::new("hi", 1100.0, 1250.0));
    }

    #[test]
    fn config_defaults_match_design() {
        let c = CaptionConfig::default();
        assert_eq!((c.max_chars_per_line, c.max_lines), (42, 2));
        assert_eq!((c.max_duration_ms, c.min_duration_ms), (6000.0, 800.0));
        assert!(!c.speaker_labels);
        assert_eq!(c.timeline_end_ms, None);
    }
}
