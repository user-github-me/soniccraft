//! SonicCraft captions: word alignment (provider timings or estimates), caption segmentation and
//! WebVTT / SRT / JSON writers (docs/DESIGN.md §5.4, §6, §10.1, §10.3).
//!
//! The crate is independent of the other SonicCraft crates: it works on plain text, millisecond times
//! and its own small input types. Everything is pure and deterministic (no I/O, no clocks, no
//! randomness, ordered collections only), so the same input always produces byte-identical files.
//!
//! # Pipeline
//!
//! 1. Per cue, compute the subtitle text with [`text::display_text`] (caption override, else spoken
//!    text without `[audio tags]`).
//! 2. Time its words with [`align::map_provider_words`] (provider timings, possibly none, plus the speech
//!    regions of the trimmed cue audio) or [`align::estimate`] (dry runs). ElevenLabs character timings
//!    are converted first with [`align::char_alignment_to_words`]. Shift the words to absolute time.
//! 3. [`build_captions`] segments every cue into [`Caption`]s.
//! 4. Write them with [`to_webvtt`], [`to_srt`], [`to_words_json_string`] and
//!    [`to_transcript_json_string`] (or the `serde_json::Value` / typed variants).
//!
//! ```
//! use soniccraft_captions::{CaptionConfig, CueInput, build_captions, to_srt, to_webvtt};
//!
//! let cues = [CueInput {
//!     cue_id: "cue_01".into(),
//!     track_id: "voice".into(),
//!     speaker: Some("Narrator".into()),
//!     text: "Autonomous software development".into(),
//!     start_ms: 0.0,
//!     end_ms: 2480.0,
//!     words: Vec::new(), // estimated from the text
//! }];
//! let config = CaptionConfig { speaker_labels: true, ..CaptionConfig::default() };
//! let captions = build_captions(&cues, &config);
//! assert_eq!(
//!     to_webvtt(&captions),
//!     "WEBVTT\n\n00:00:00.000 --> 00:00:02.480\n<v Narrator>Autonomous software development\n"
//! );
//! assert_eq!(to_srt(&captions), "1\n00:00:00,000 --> 00:00:02,480\nNarrator: Autonomous software development\n");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod align;
pub mod json;
pub mod model;
pub mod segment;
pub mod text;
pub mod writers;

pub use align::{
    Alignment, AlignmentSource, CharAlignment, char_alignment_to_words, estimate, fold_punctuation,
    map_provider_words,
};
pub use json::{
    CaptionRecord, TranscriptWord, WordRecord, WordsDocument, to_transcript_json,
    to_transcript_json_string, to_words_json, to_words_json_string, transcript, words_document,
};
pub use model::{Caption, CaptionConfig, CueInput, Interval, Word};
pub use segment::{auto_speaker_labels, build_captions};
pub use writers::{srt_timestamp, to_srt, to_webvtt, vtt_timestamp};
