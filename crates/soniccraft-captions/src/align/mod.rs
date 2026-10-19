//! Word alignment: turning provider timings (or audio analysis) into timed display words.
//!
//! * [`map_provider_words`] — provider word timings → display tokens (robust to tokenization, case,
//!   punctuation, number normalization and missing/extra words; estimates what does not match).
//! * [`estimate`](fn@estimate) — no provider timings: speech regions + syllable weights (DESIGN §5.4).
//! * [`char_alignment_to_words`] / [`CharAlignment`] — ElevenLabs per-character timings → words, audio tags
//!   dropped.
//! * [`fold_punctuation`] — fold punctuation-only provider words (Kokoro) into their neighbours.
//!
//! # Typical engine flow for one cue
//!
//! ```
//! use soniccraft_captions::{Interval, Word, align::map_provider_words, text::display_text};
//!
//! // Provider timings relative to the trimmed cue audio (shifted by the trim offset already).
//! let provider = vec![Word::new("Hello", 30.0, 400.0), Word::new("world", 450.0, 900.0)];
//! let cue_audio = Interval::new(0.0, 960.0);
//! let regions = [Interval::new(30.0, 930.0)]; // speech regions detected in the trimmed audio
//! let display = display_text("Hello, world!", None);
//! let alignment = map_provider_words(&display, &provider, cue_audio, &regions);
//! assert!(!alignment.is_estimated());
//! // Shift to absolute time before building captions.
//! let cue_start_ms = 12_000.0;
//! let words: Vec<Word> = alignment.words.iter().map(|w| w.shifted(cue_start_ms)).collect();
//! assert_eq!(words[0], Word::new("Hello,", 12_030.0, 12_400.0));
//! ```

mod chars;
mod estimate;
mod provider;

use serde::{Deserialize, Serialize};

pub use chars::{CharAlignment, char_alignment_to_words, fold_punctuation};
pub use estimate::estimate;
pub use provider::map_provider_words;

use crate::model::Word;

/// Where the word timings of an [`Alignment`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlignmentSource {
    /// Every display word was matched to provider timings.
    Provider,
    /// Some words were matched, the rest estimated.
    Mixed,
    /// All words were estimated (no usable provider timings, or nothing matched).
    Estimated,
}

/// Timed display words of one cue.
#[derive(Debug, Clone, PartialEq)]
pub struct Alignment {
    /// One word per display token, in text order, with non-decreasing, non-overlapping times.
    pub words: Vec<Word>,
    /// Where the timings came from.
    pub source: AlignmentSource,
    /// Number of words matched to provider timings.
    pub matched: usize,
}

impl Alignment {
    /// True when at least one word's timing is an estimate (the engine reports `alignment_estimated`).
    pub fn is_estimated(&self) -> bool {
        self.source != AlignmentSource::Provider
    }
}
