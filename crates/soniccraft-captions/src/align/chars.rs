//! Character-level timings (ElevenLabs `/with-timestamps` alignment) → word timings, and folding of
//! punctuation-only provider words (Kokoro captioned speech).

use serde::{Deserialize, Serialize};

use crate::model::Word;
use crate::text::{is_spoken, tag_end};

/// ElevenLabs-style character alignment, with the field names of the provider's JSON
/// (`alignment` / `normalized_alignment` of `POST /v1/text-to-speech/{voice_id}/with-timestamps`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CharAlignment {
    /// One entry per character (usually a single `char`; multi-character entries are allowed).
    pub characters: Vec<String>,
    /// Start of each character in seconds.
    pub character_start_times_seconds: Vec<f64>,
    /// End of each character in seconds.
    pub character_end_times_seconds: Vec<f64>,
}

impl CharAlignment {
    /// Group the characters into words, see [`char_alignment_to_words`].
    pub fn to_words(&self) -> Vec<Word> {
        char_alignment_to_words(
            &self.characters,
            &self.character_start_times_seconds,
            &self.character_end_times_seconds,
        )
    }
}

/// Group per-character timings (in **seconds**) into words (in **milliseconds**).
///
/// A word is a maximal run of non-whitespace entries; its start is the earliest finite character start
/// and its end the latest finite character end. Bracketed audio tags (`[whispering]`, even when they
/// contain spaces) and SSML-style tags (see [`crate::text::strip_audio_tags`]) are dropped and separate
/// words. Words without any finite time are dropped (the mapping estimates them). When the three slices
/// differ in length only the common prefix is used.
pub fn char_alignment_to_words<S: AsRef<str>>(
    characters: &[S],
    start_times_s: &[f64],
    end_times_s: &[f64],
) -> Vec<Word> {
    let n = characters
        .len()
        .min(start_times_s.len())
        .min(end_times_s.len());
    // Flatten the entries to find tags that span several entries.
    let mut flat: Vec<char> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    for (i, entry) in characters[..n].iter().enumerate() {
        for c in entry.as_ref().chars() {
            flat.push(c);
            owner.push(i);
        }
    }
    let mut in_tag = vec![false; n];
    let mut k = 0;
    while k < flat.len() {
        match tag_end(&flat, k) {
            Some(end) => {
                for &o in &owner[k..end] {
                    in_tag[o] = true;
                }
                k = end;
            }
            None => k += 1,
        }
    }

    let mut words = Vec::new();
    let mut current = WordBuilder::default();
    for i in 0..n {
        let entry = characters[i].as_ref();
        if entry.is_empty() {
            continue;
        }
        if in_tag[i] || entry.chars().all(char::is_whitespace) {
            current.finish(&mut words);
            continue;
        }
        current.push(entry, start_times_s[i], end_times_s[i]);
    }
    current.finish(&mut words);
    words
}

#[derive(Default)]
struct WordBuilder {
    text: String,
    start: Option<f64>,
    end: Option<f64>,
}

impl WordBuilder {
    fn push(&mut self, entry: &str, start_s: f64, end_s: f64) {
        self.text.push_str(entry);
        if start_s.is_finite() {
            self.start = Some(self.start.map_or(start_s, |s| s.min(start_s)));
        }
        if end_s.is_finite() {
            self.end = Some(self.end.map_or(end_s, |e| e.max(end_s)));
        }
    }

    fn finish(&mut self, words: &mut Vec<Word>) {
        let builder = std::mem::take(self);
        let (start, end) = match (builder.start, builder.end) {
            (Some(s), Some(e)) => (s, e.max(s)),
            (Some(s), None) => (s, s),
            (None, Some(e)) => (e, e),
            (None, None) => return,
        };
        if !builder.text.is_empty() {
            words.push(Word::new(builder.text, start * 1000.0, end * 1000.0));
        }
    }
}

/// Fold provider words without speakable content (`","`, `"."`, `"—"`) into the previous word's text
/// (into the next word when there is no previous one). Times of the kept words are unchanged: the
/// punctuation's own time slot is usually a pause and is not attributed to any word.
pub fn fold_punctuation(words: &[Word]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::with_capacity(words.len());
    let mut pending = String::new();
    for w in words {
        if is_spoken(&w.text) {
            let mut word = w.clone();
            if !pending.is_empty() {
                word.text = format!("{}{}", std::mem::take(&mut pending), word.text);
            }
            out.push(word);
        } else if let Some(last) = out.last_mut() {
            last.text.push_str(&w.text);
        } else {
            pending.push_str(&w.text);
        }
    }
    if !pending.is_empty() && out.is_empty() {
        // Only punctuation: nothing to fold into.
        return words.to_vec();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<String> {
        text.chars().map(String::from).collect()
    }

    fn times(n: usize, step: f64) -> (Vec<f64>, Vec<f64>) {
        (
            (0..n).map(|i| i as f64 * step).collect(),
            (0..n).map(|i| (i + 1) as f64 * step).collect(),
        )
    }

    #[test]
    fn groups_on_whitespace() {
        let c = chars("Hi there");
        let (s, e) = times(c.len(), 0.1);
        let words = char_alignment_to_words(&c, &s, &e);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hi");
        assert!((words[0].start_ms - 0.0).abs() < 1e-9 && (words[0].end_ms - 200.0).abs() < 1e-9);
        assert_eq!(words[1].text, "there");
        assert!((words[1].start_ms - 300.0).abs() < 1e-9 && (words[1].end_ms - 800.0).abs() < 1e-9);
    }

    #[test]
    fn drops_audio_tags_even_with_spaces() {
        let c = chars("[whispering softly] Don't[laughs]go <break time=\"1s\"/> now");
        let (s, e) = times(c.len(), 0.05);
        let texts: Vec<String> = char_alignment_to_words(&c, &s, &e)
            .into_iter()
            .map(|w| w.text)
            .collect();
        assert_eq!(texts, ["Don't", "go", "now"]);
    }

    #[test]
    fn struct_matches_provider_json() {
        let json = r#"{"characters":["H","i"],"character_start_times_seconds":[0.0,0.1],"character_end_times_seconds":[0.1,0.25]}"#;
        let alignment: CharAlignment = serde_json::from_str(json).expect("valid alignment JSON");
        let words = alignment.to_words();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "Hi");
        assert!((words[0].end_ms - 250.0).abs() < 1e-9);
    }

    #[test]
    fn tolerates_bad_input() {
        let c = chars("ab cd ef");
        let s = vec![0.0, 0.1, 0.2, f64::NAN, f64::NAN, 0.5];
        let e = vec![0.1, 0.2, 0.3, f64::NAN, f64::NAN, 0.6, 0.7];
        let words = char_alignment_to_words(&c, &s, &e);
        // Length mismatch: only the first 6 entries ("ab cd ") are used; "cd" has no finite time.
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "ab");
        let empty: Vec<String> = Vec::new();
        assert!(char_alignment_to_words(&empty, &[], &[]).is_empty());
        let multi = vec!["He".to_string(), String::new(), "y".to_string()];
        let words = char_alignment_to_words(&multi, &[0.0, 0.1, 0.2], &[0.1, 0.2, 0.3]);
        assert_eq!(words[0].text, "Hey");
    }

    #[test]
    fn folds_punctuation() {
        let words = [
            Word::new("“", 0.0, 0.0),
            Word::new("Hello", 0.0, 300.0),
            Word::new(",", 300.0, 500.0),
            Word::new("world", 500.0, 800.0),
            Word::new("!", 800.0, 900.0),
        ];
        let folded = fold_punctuation(&words);
        assert_eq!(
            folded,
            [
                Word::new("“Hello,", 0.0, 300.0),
                Word::new("world!", 500.0, 800.0)
            ]
        );
        let only = [Word::new("…", 0.0, 100.0)];
        assert_eq!(fold_punctuation(&only), only);
    }
}
