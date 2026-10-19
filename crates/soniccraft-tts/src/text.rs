//! Text utilities shared by the duration estimator (DESIGN.md §5.3), the mock voice and word-timing
//! post-processing.

/// Longest bracketed segment (in bytes, brackets excluded) treated as an audio tag.
const MAX_TAG_BYTES: usize = 64;

/// Remove ElevenLabs-style `[audio tags]` such as `[whispering]` or `[laughs]`.
///
/// A tag is `[` followed by 1–64 bytes without `[`, `]` or a newline, then `]`. Anything else (an unmatched
/// `[`, an over-long bracket) is kept verbatim.
pub fn strip_audio_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        match after.find(['[', ']', '\n']) {
            Some(close)
                if after.as_bytes()[close] == b']' && close > 0 && close <= MAX_TAG_BYTES =>
            {
                out.push_str(&rest[..open]);
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whitespace-separated tokens of `text`, with audio tags removed.
pub fn tokens(text: &str) -> Vec<String> {
    strip_audio_tags(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// `text` with audio tags kept and every whitespace run (including newlines) collapsed to one space.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when a token contains a letter or digit (punctuation-only tokens are not spoken words).
pub fn is_spoken_token(token: &str) -> bool {
    token.chars().any(char::is_alphanumeric)
}

/// Number of spoken words in `text` (audio tags and punctuation-only tokens excluded).
pub fn spoken_word_count(text: &str) -> usize {
    tokens(text).iter().filter(|t| is_spoken_token(t)).count()
}

/// Characters that take speaking time (DESIGN.md §5.3): letters, digits, punctuation and single spaces
/// between words. Audio tags and control characters are excluded, and whitespace runs count once.
pub fn spoken_chars(text: &str) -> usize {
    let stripped = strip_audio_tags(text);
    let mut count = 0;
    let mut pending_space = false;
    let mut started = false;
    for c in stripped.chars() {
        if c.is_whitespace() {
            pending_space = started;
            continue;
        }
        if c.is_control() {
            continue;
        }
        if pending_space {
            count += 1;
            pending_space = false;
        }
        count += 1;
        started = true;
    }
    count
}

fn is_vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e' | 'i' | 'o' | 'u' | 'y'
            | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'æ' | 'è' | 'é' | 'ê' | 'ë' | 'ì' | 'í' | 'î' | 'ï'
            | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ù' | 'ú' | 'û' | 'ü' | 'ý' | 'ÿ' | 'œ' | 'ą' | 'ę'
            | 'ő' | 'ű' | 'ů' | 'ě'
            // Cyrillic and Greek vowels
            | 'а' | 'е' | 'ё' | 'и' | 'о' | 'у' | 'ы' | 'э' | 'ю' | 'я' | 'і' | 'ї' | 'є'
            | 'α' | 'ε' | 'η' | 'ι' | 'ο' | 'υ' | 'ω' | 'ά' | 'έ' | 'ή' | 'ί' | 'ό' | 'ύ' | 'ώ'
    )
}

/// Scripts where one character is roughly one syllable (CJK ideographs, kana, Hangul syllables).
fn is_syllabic_char(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xAC00..=0xD7AF // Hangul syllables
        | 0xF900..=0xFAFF) // CJK compatibility ideographs
}

/// Heuristic syllable count of one word: vowel groups for alphabetic scripts (with a silent final English
/// `e`), one per CJK/kana/Hangul character, one per digit. At least 1 for any token with a letter or digit;
/// 0 for punctuation-only tokens.
pub fn syllables(word: &str) -> usize {
    if !is_spoken_token(word) {
        return 0;
    }
    let lower: Vec<char> = word.chars().flat_map(char::to_lowercase).collect();
    let letters: Vec<char> = lower
        .iter()
        .copied()
        .filter(|c| c.is_alphabetic())
        .collect();
    let mut count = 0usize;
    let mut prev_vowel = false;
    for &c in &letters {
        if is_syllabic_char(c) {
            count += 1;
            prev_vowel = false;
            continue;
        }
        let vowel = is_vowel(c);
        if vowel && !prev_vowel {
            count += 1;
        }
        prev_vowel = vowel;
    }
    // Silent final "e" ("make", "voice"), but not "-le" after a consonant ("table") and never below one.
    let n = letters.len();
    if n >= 3 && count > 1 && letters[n - 1] == 'e' && !is_vowel(letters[n - 2]) {
        let le_syllable = letters[n - 2] == 'l' && !is_vowel(letters[n - 3]);
        if !le_syllable {
            count -= 1;
        }
    }
    count += word.chars().filter(char::is_ascii_digit).count();
    count.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_audio_tags() {
        assert_eq!(
            strip_audio_tags("[whispering] Hello [laughs] world"),
            " Hello  world"
        );
        assert_eq!(strip_audio_tags("no tags"), "no tags");
        assert_eq!(strip_audio_tags("unclosed [bracket"), "unclosed [bracket");
        assert_eq!(strip_audio_tags("empty [] stays"), "empty [] stays");
        assert_eq!(strip_audio_tags("nested [a [b] c]"), "nested [a  c]");
        assert_eq!(strip_audio_tags("multi\n[line\n] x"), "multi\n[line\n] x");
        let long = format!("[{}]", "x".repeat(65));
        assert_eq!(strip_audio_tags(&long), long);
        assert_eq!(strip_audio_tags("ünï[cödé]x"), "ünïx");
    }

    #[test]
    fn counts_spoken_characters() {
        assert_eq!(spoken_chars("Hello, world."), 13);
        assert_eq!(spoken_chars("  Hello \n\n  world  "), 11);
        assert_eq!(spoken_chars("[excited] Hi!"), 3);
        assert_eq!(spoken_chars(""), 0);
        assert_eq!(spoken_chars("[sigh]"), 0);
        assert_eq!(spoken_chars("naïve café"), 10);
    }

    #[test]
    fn counts_words() {
        assert_eq!(spoken_word_count("Hello — world , again [pause] !"), 3);
        assert_eq!(collapse_whitespace(" a\n b\t\tc "), "a b c");
    }

    #[test]
    fn syllable_heuristic() {
        let cases = [
            ("a", 1),
            ("the", 1),
            ("make", 1),
            ("voice", 1),
            ("table", 2),
            ("hello", 2),
            ("world.", 1),
            ("autonomous", 4),
            ("software", 2),
            ("development", 4),
            ("rhythm", 1),
            ("2026", 4),
            ("café", 2),
            ("日本語", 3),
            ("—", 0),
            ("...", 0),
            ("x", 1),
        ];
        for (word, expected) in cases {
            assert_eq!(syllables(word), expected, "{word}");
        }
    }
}
