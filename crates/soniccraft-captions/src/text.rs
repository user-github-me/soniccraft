//! Text processing for captions: whitespace normalization, TTS markup stripping, tokenization into
//! display words, matching keys, phrase-break detection and syllable estimates.
//!
//! # Tokens
//!
//! [`tokenize`] splits a display text into [`Token`]s, the unit of word timing and line breaking:
//!
//! * Tokens are separated by whitespace. No-break spaces (U+00A0, U+2007, U+202F) do not separate tokens.
//! * Punctuation stays attached to its word: `"Hello,"`, `"(see"`, `"don't"`, `"state-of-the-art"`,
//!   `"3.14"`, `"$3,000"`, `"great🎉"`.
//! * Unspaced dashes and ellipses join two tokens without a space: `"wait—what"` → `"wait—"` + `"what"`
//!   (also `--`, `―`, `…`, `...`, and `–` except between digits as in `1990–2000`).
//! * CJK ideographs and kana are one token per character; closing punctuation and small kana attach to
//!   the preceding character (`"好，"`, `"きょ"`), opening brackets to the following one (`"「世"`).
//!   Hangul, Thai and other scripts keep their whitespace-delimited words.
//! * A zero-width space (U+200B) separates tokens without a visible space.
//! * Tokens without anything speakable (`—`, `…`, `«`, `🎉` standing alone) are merged into a neighbour:
//!   opening punctuation into the next token, everything else into the previous one (`"Quoi ?"`,
//!   `"Wait —"`). A text consisting only of such characters stays a single token.
//!
//! [`Token::space_before`] records whether a space separated the token from the previous one, so the
//! original text can be rebuilt exactly (modulo whitespace normalization).

use unicode_segmentation::UnicodeSegmentation;

/// Zero-width space: a break opportunity without a visible separator.
pub(crate) const ZWSP: char = '\u{200B}';

/// Strength of the phrase break after a token, from its trailing punctuation. Ordered
/// `None < Comma < Clause < Sentence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum BreakStrength {
    /// No punctuation.
    #[default]
    None,
    /// Comma-like: `,` `、` `，` `،`.
    Comma,
    /// Clause-level: `;` `:` dashes, and ellipses not followed by a capitalized word.
    Clause,
    /// Sentence end: `.` (except abbreviations and initials), `!` `?` `。` `！` `？` `।`, ellipses followed
    /// by a capitalized word.
    Sentence,
}

/// One display token (see the module docs for the rules).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// Display text including attached punctuation.
    pub text: String,
    /// True when a space separated this token from the previous one (always false for the first token).
    pub space_before: bool,
    /// Normalized key for matching against provider words (see [`match_key`]).
    pub key: String,
    /// Estimated syllables (0 only for a text without anything speakable).
    pub syllables: u32,
    /// Relative speaking-time weight (syllable-based, see [`token_weight`]).
    pub weight: f64,
    /// Phrase break after this token.
    pub break_after: BreakStrength,
}

/// Collapse every run of (breaking) whitespace into one space and trim both ends. No-break spaces are
/// kept; soft hyphens, BOMs, word joiners and control characters are removed.
pub fn normalize_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if is_breaking_space(c) {
            pending_space = !out.is_empty();
        } else if is_ignorable(c) {
            continue;
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Number of user-perceived characters (extended grapheme clusters). This is the unit of
/// `max_chars_per_line`.
pub fn display_len(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Remove TTS markup that must not appear in subtitles: bracketed audio tags (`[whispering]`,
/// `[laughs]`, `[pause:0.5s]`), macOS `say` commands (`[[slnc 500]]`) and SSML-style tags
/// (`<break time="1s"/>`, `<phoneme …>`/`</phoneme>` — inner text is kept). A tag between two words
/// becomes a space (`"Hello[pause]world"` → `"Hello world"`); a tag next to whitespace or before closing
/// punctuation simply disappears. The result is **not** whitespace-normalized (see [`display_text`]).
///
/// A bracketed tag is at most 100 characters on one line, without nested brackets.
pub fn strip_audio_tags(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = tag_end(&chars, i) {
            let after_word = out.chars().next_back().is_some_and(|c| !c.is_whitespace());
            let before_word = chars
                .get(end)
                .is_some_and(|&c| !c.is_whitespace() && !is_close_punct(c));
            if after_word && before_word && tag_end(&chars, end).is_none() {
                out.push(' ');
            }
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The subtitle text of a cue: its `caption` override verbatim when set, otherwise its spoken `text`
/// without audio tags (see [`strip_audio_tags`]); whitespace-normalized in both cases.
pub fn display_text(text: &str, caption: Option<&str>) -> String {
    match caption {
        Some(caption) => normalize_whitespace(caption),
        None => normalize_whitespace(&strip_audio_tags(text)),
    }
}

/// SSML-style tag names stripped by [`strip_audio_tags`] (lowercase).
const MARKUP_TAGS: &[&str] = &[
    "break", "emotion", "emphasis", "lang", "mark", "p", "phoneme", "prosody", "s", "say-as",
    "speak", "speed", "spell", "sub", "voice", "volume",
];

/// If a TTS markup tag starts at `chars[i]`, the index just past it.
pub(crate) fn tag_end(chars: &[char], i: usize) -> Option<usize> {
    match chars.get(i)? {
        '[' => {
            if chars.get(i + 1) == Some(&'[') {
                // [[say command]]
                let limit = (i + 202).min(chars.len());
                let mut j = i + 2;
                while j + 1 < limit {
                    match chars[j] {
                        '\n' | '\r' => return None,
                        ']' if chars[j + 1] == ']' => return (j > i + 2).then_some(j + 2),
                        _ => j += 1,
                    }
                }
                None
            } else {
                let limit = (i + 102).min(chars.len());
                for (j, &c) in chars.iter().enumerate().take(limit).skip(i + 1) {
                    match c {
                        ']' => return (j > i + 1).then_some(j + 1),
                        '[' | '\n' | '\r' => return None,
                        _ => {}
                    }
                }
                None
            }
        }
        '<' => {
            let mut j = i + 1;
            if chars.get(j) == Some(&'/') {
                j += 1;
            }
            let name_start = j;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '-' || chars[j] == '_')
            {
                j += 1;
            }
            if j == name_start || !chars[name_start].is_ascii_alphabetic() {
                return None;
            }
            let name: String = chars[name_start..j]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            if !MARKUP_TAGS.contains(&name.as_str()) {
                return None;
            }
            // The name must be followed by whitespace, `/` or `>`.
            match chars.get(j) {
                Some(c) if c.is_whitespace() || *c == '/' || *c == '>' => {}
                _ => return None,
            }
            let limit = (i + 302).min(chars.len());
            for (k, &c) in chars.iter().enumerate().take(limit).skip(j) {
                match c {
                    '>' => return Some(k + 1),
                    '<' | '\n' | '\r' => return None,
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// Split a display text into tokens (see the module docs).
pub fn tokenize(text: &str) -> Vec<Token> {
    let normalized = normalize_whitespace(text);
    let mut pieces = Vec::new();
    for (k, chunk) in normalized.split(' ').enumerate() {
        if !chunk.is_empty() {
            split_chunk(chunk, k > 0, &mut pieces);
        }
    }
    let pieces = merge_unspoken(pieces);
    let mut tokens = Vec::with_capacity(pieces.len());
    for (i, piece) in pieces.iter().enumerate() {
        let next = pieces.get(i + 1).map(|p| p.text.as_str());
        let syllables = syllables(&piece.text);
        tokens.push(Token {
            text: piece.text.clone(),
            space_before: i > 0 && piece.space_before,
            key: match_key(&piece.text),
            syllables,
            weight: token_weight(&piece.text, syllables),
            break_after: break_strength(&piece.text, next),
        });
    }
    tokens
}

/// A token before metadata is computed.
#[derive(Debug, Clone)]
struct Piece {
    text: String,
    space_before: bool,
}

/// Grapheme classes used for splitting chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// A CJK ideograph or kana that can start a token.
    Cjk,
    /// Small kana, prolonged sound mark, iteration marks: never start a token.
    NonStarter,
    /// Opening brackets and quotes.
    Open,
    /// Closing brackets, quotes and punctuation.
    Close,
    /// Zero-width space.
    Joint,
    Other,
}

fn base_class(c: char) -> Class {
    if c == ZWSP {
        Class::Joint
    } else if is_non_starter(c) {
        Class::NonStarter
    } else if is_cjk_char(c) {
        Class::Cjk
    } else if is_open_punct(c) {
        Class::Open
    } else if is_close_punct(c) {
        Class::Close
    } else {
        Class::Other
    }
}

/// Split one whitespace-free chunk into pieces at CJK, dash-join and zero-width-space boundaries.
fn split_chunk(chunk: &str, space_before: bool, out: &mut Vec<Piece>) {
    let graphemes: Vec<&str> = chunk.graphemes(true).collect();
    let firsts: Vec<char> = graphemes
        .iter()
        .map(|g| g.chars().next().unwrap_or(' '))
        .collect();
    let n = graphemes.len();
    let has_cjk = firsts
        .iter()
        .any(|&c| matches!(base_class(c), Class::Cjk | Class::NonStarter));
    // In CJK text, ASCII double quotes alternate between opening and closing.
    let mut quote_opens = true;
    let classes: Vec<Class> = firsts
        .iter()
        .map(|&c| {
            if has_cjk && c == '"' {
                let class = if quote_opens {
                    Class::Open
                } else {
                    Class::Close
                };
                quote_opens = !quote_opens;
                class
            } else {
                base_class(c)
            }
        })
        .collect();

    let mut split_before = vec![false; n];
    for i in 1..n {
        split_before[i] = match (classes[i - 1], classes[i]) {
            (Class::Joint, _) | (_, Class::Joint) | (_, Class::Cjk) => true,
            (_, Class::Close | Class::NonStarter) => false,
            (Class::Cjk | Class::NonStarter, _) => true,
            (Class::Close, _) if close_run_follows_cjk(&classes, i - 1) => true,
            _ => dash_join_break(&graphemes, &firsts, i),
        };
    }
    // Opening punctuation belongs to the following token: move each split before its run of openers.
    for i in 1..n {
        if split_before[i] && classes[i - 1] == Class::Open {
            let mut j = i - 1;
            while j > 0 && classes[j - 1] == Class::Open {
                j -= 1;
            }
            split_before[i] = false;
            if j > 0 {
                split_before[j] = true;
            }
        }
    }

    let mut current = String::new();
    let mut first_piece = true;
    let mut flush = |current: &mut String, out: &mut Vec<Piece>| {
        if !current.is_empty() {
            out.push(Piece {
                text: std::mem::take(current),
                space_before: first_piece && space_before,
            });
            first_piece = false;
        }
    };
    for i in 0..n {
        if i > 0 && split_before[i] {
            flush(&mut current, out);
        }
        if classes[i] != Class::Joint {
            current.push_str(graphemes[i]);
        }
    }
    flush(&mut current, out);
}

/// True when the run of closing punctuation ending at `k` directly follows a CJK character.
fn close_run_follows_cjk(classes: &[Class], k: usize) -> bool {
    let mut j = k;
    loop {
        match classes[j] {
            Class::Close if j > 0 => j -= 1,
            Class::Cjk | Class::NonStarter => return true,
            _ => return false,
        }
    }
}

/// Split after an unspaced dash or ellipsis when a word follows (`wait—what`, `so...then`).
fn dash_join_break(graphemes: &[&str], firsts: &[char], i: usize) -> bool {
    let cur = firsts[i];
    if !(cur.is_alphanumeric() || is_open_punct(cur) || cur == '"' || cur == '\'') {
        return false;
    }
    if graphemes[i - 1].chars().count() != 1 {
        return false;
    }
    match firsts[i - 1] {
        '—' | '―' | '…' => true,
        '–' => !(i >= 2 && firsts[i - 2].is_ascii_digit() && cur.is_ascii_digit()),
        '-' => i >= 2 && firsts[i - 2] == '-',
        '.' => i >= 3 && firsts[i - 2] == '.' && firsts[i - 3] == '.',
        _ => false,
    }
}

/// Merge pieces without speakable content into their neighbours (see the module docs).
fn merge_unspoken(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    // Unspoken text waiting to be prepended to the next spoken piece.
    let mut prefix: Option<Piece> = None;
    for piece in pieces {
        if is_spoken(&piece.text) {
            let mut piece = piece;
            if let Some(pre) = prefix.take() {
                let sep = if piece.space_before { " " } else { "" };
                piece.text = format!("{}{sep}{}", pre.text, piece.text);
                piece.space_before = pre.space_before;
            }
            out.push(piece);
            continue;
        }
        let opening = piece
            .text
            .chars()
            .next()
            .is_some_and(|c| is_open_punct(c) || c == '"');
        if let Some(pre) = prefix.as_mut() {
            // Keep document order: once something waits for the next word, later marks join it.
            if piece.space_before {
                pre.text.push(' ');
            }
            pre.text.push_str(&piece.text);
        } else if !opening && let Some(last) = out.last_mut() {
            if piece.space_before {
                last.text.push(' ');
            }
            last.text.push_str(&piece.text);
        } else {
            prefix = Some(piece);
        }
    }
    if let Some(pre) = prefix {
        match out.last_mut() {
            Some(last) => {
                if pre.space_before {
                    last.text.push(' ');
                }
                last.text.push_str(&pre.text);
            }
            None => out.push(pre),
        }
    }
    out
}

/// True when `text` contains something a TTS engine would speak (a letter, digit or spoken symbol).
pub(crate) fn is_spoken(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_alphanumeric() || symbol_syllables(c) > 0)
}

/// Normalized matching key: lowercase letters and digits only, with `&` → `and`, `%` → `percent`,
/// `+` → `plus`, `@` → `at`. Apostrophes, hyphens, periods, quotes and other symbols are dropped, so
/// `"Don't,"`, `"dont"` and `"DON’T"` all become `dont`.
pub fn match_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            match c {
                '&' => out.push_str("and"),
                '%' => out.push_str("percent"),
                '+' => out.push_str("plus"),
                '@' => out.push_str("at"),
                _ => {}
            }
        }
    }
    out
}

/// Speaking-time weight of a token: `syllables + 0.2 × max(0, letters − 3 × syllables)`, so long
/// consonant-heavy words ("strengths") weigh more than short ones with the same syllable count.
/// Unspoken tokens weigh 0.
pub fn token_weight(text: &str, syllables: u32) -> f64 {
    if syllables == 0 {
        return 0.0;
    }
    let letters = text
        .chars()
        .filter(|&c| c.is_alphabetic() && !is_cjk_char(c) && !is_hangul(c))
        .count();
    let s = f64::from(syllables);
    s + 0.2 * (letters as f64 - 3.0 * s).max(0.0)
}

// ---- phrase breaks ------------------------------------------------------------------------------------

/// Abbreviations that never end a sentence (lowercase, without the final period).
const ABBREVIATIONS: &[&str] = &[
    "adm", "al", "apr", "approx", "assn", "aug", "ave", "blvd", "bros", "ca", "capt", "cf", "ch",
    "chap", "cmdr", "col", "cpt", "dec", "dept", "dr", "feb", "fig", "figs", "fr", "ft", "gen",
    "gov", "hon", "jan", "jr", "jul", "jun", "lt", "mar", "mr", "mrs", "ms", "mt", "mx", "no",
    "nos", "nov", "nr", "oct", "p", "pp", "pres", "prof", "rep", "rev", "sen", "sep", "sept",
    "sgt", "sr", "st", "supt", "univ", "v", "vol", "vols", "vs",
];

/// Phrase-break strength after a token, from its trailing punctuation (closing quotes and brackets are
/// skipped). `next` is the following token, used to tell sentence ends from abbreviations and ellipses.
pub fn break_strength(text: &str, next: Option<&str>) -> BreakStrength {
    let trimmed = text.trim_end_matches(|c: char| is_trailing_closer(c));
    let Some(last) = trimmed.chars().next_back() else {
        return BreakStrength::None;
    };
    match last {
        '!' | '?' | '‼' | '⁇' | '⁈' | '⁉' | '‽' | '。' | '！' | '？' | '｡' | '؟' | '।' | '॥' => {
            BreakStrength::Sentence
        }
        '…' | '‥' => ellipsis_strength(next),
        '.' if trimmed.ends_with("...") => ellipsis_strength(next),
        '.' => period_strength(&trimmed[..trimmed.len() - 1], next),
        ';' | ':' | '；' | '：' | '؛' | '—' | '―' | '–' => BreakStrength::Clause,
        '-' if trimmed.ends_with("--") => BreakStrength::Clause,
        ',' | '、' | '，' | '､' | '،' | '﹐' | '﹑' => BreakStrength::Comma,
        _ => BreakStrength::None,
    }
}

/// Closing quotes and brackets skipped when looking for a token's final punctuation.
const TRAILING_CLOSERS: &str = "\"')]}”’»›」』）】〕〉》〗〙〛］｝｣〟";

fn is_trailing_closer(c: char) -> bool {
    TRAILING_CLOSERS.contains(c)
}

/// True when `next` starts like a new sentence: its first alphanumeric character is not lowercase.
fn starts_sentence(next: Option<&str>) -> bool {
    match next {
        None => true,
        Some(next) => next
            .chars()
            .find(|c| c.is_alphanumeric())
            .is_none_or(|c| !c.is_lowercase()),
    }
}

fn ellipsis_strength(next: Option<&str>) -> BreakStrength {
    if starts_sentence(next) {
        BreakStrength::Sentence
    } else {
        BreakStrength::Clause
    }
}

/// A final period: initials and known abbreviations do not end sentences; dotted abbreviations (`e.g.`,
/// `U.S.`) only when the next token starts a sentence.
fn period_strength(core: &str, next: Option<&str>) -> BreakStrength {
    let core = core.trim_start_matches(|c: char| !c.is_alphanumeric());
    let mut chars = core.chars();
    if let (Some(first), None) = (chars.next(), chars.next())
        && first.is_alphabetic()
    {
        return BreakStrength::None; // an initial: "J."
    }
    let lower = core.to_lowercase();
    if ABBREVIATIONS.contains(&lower.as_str()) {
        return BreakStrength::None;
    }
    let dotted = core.contains('.')
        && core.split('.').all(|seg| {
            !seg.is_empty() && seg.chars().count() <= 3 && seg.chars().all(char::is_alphabetic)
        });
    if dotted && !starts_sentence(next) {
        return BreakStrength::None;
    }
    BreakStrength::Sentence
}

// ---- character classes ---------------------------------------------------------------------------------

fn is_breaking_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{00A0}' | '\u{2007}' | '\u{202F}')
}

fn is_ignorable(c: char) -> bool {
    matches!(c, '\u{00AD}' | '\u{FEFF}' | '\u{2060}') || (c.is_control() && !c.is_whitespace())
}

/// CJK ideographs, kana and bopomofo: scripts written without spaces, one token per character.
pub(crate) fn is_cjk_char(c: char) -> bool {
    is_han(c) || is_kana(c) || matches!(c as u32, 0x3100..=0x312F | 0x31A0..=0x31BF)
}

fn is_han(c: char) -> bool {
    matches!(
        c as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
            | 0x30000..=0x323AF
            | 0x3005..=0x3007
            | 0x3021..=0x3029
    )
}

fn is_kana(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x309F | 0x30A0..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F)
}

fn is_hangul(c: char) -> bool {
    matches!(c as u32, 0xAC00..=0xD7A3 | 0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xD7B0..=0xD7FF)
}

/// Characters that must not start a line or token in Japanese: small kana, prolonged sound marks,
/// iteration marks and sound marks.
const NON_STARTERS: &str = "ぁぃぅぇぉっゃゅょゎゕゖァィゥェォッャュョヮヵヶーゝゞヽヾ々゛゜ｰﾞﾟ";

fn is_non_starter(c: char) -> bool {
    NON_STARTERS.contains(c) || matches!(c as u32, 0x31F0..=0x31FF | 0xFF67..=0xFF6F)
}

/// Opening brackets and quotes: they attach to the following token.
const OPEN_PUNCT: &str = "([{“‘„‚«‹¿¡「『（【〔〈《〖〘〚［｛｢〝";

fn is_open_punct(c: char) -> bool {
    OPEN_PUNCT.contains(c)
}

/// Closing punctuation: it attaches to the preceding token and never starts a CJK token.
const CLOSE_PUNCT: &str = ".,;:!?)]}。、，．！？；：」』）】〕〉》〗〙〛］｝”’»›…‥・｡､｣〟";

fn is_close_punct(c: char) -> bool {
    CLOSE_PUNCT.contains(c)
}

/// Combining marks, variation selectors and joiners (they extend the preceding letter).
fn is_mark(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0591..=0x05BD
            | 0x0610..=0x061A
            | 0x064B..=0x065F
            | 0x0670
            | 0x093C
            | 0x094D
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200C..=0x200D
            | 0x20D0..=0x20FF
            | 0x3099..=0x309A
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
    )
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '’' | 'ʼ')
}

// ---- syllables --------------------------------------------------------------------------------------------

/// Estimated number of syllables spoken for `text` (a word, a token or a whole phrase).
///
/// Heuristics, deterministic and language-agnostic where possible:
/// * Latin script: vowel groups, with English silent-`e` / `-es` / `-ed` rules for ASCII words; short
///   all-caps acronyms are spelled out (`SQL` = 3); vowel-less runs are spelled out (`km` = 2), except
///   interjections like `hmm`.
/// * Cyrillic: vowel letters; Greek: vowel groups; Hangul: one per syllable block; CJK ideographs: one per
///   character; kana: one per mora (small kana count 0, `っ` and `ー` count 1); other scripts:
///   `letters / 2.5`.
/// * Numbers are read in English: `1999` = "nineteen ninety-nine" (5), `2,500` (6), `3.14` = "three
///   point one four" (4), `10:30` (3); leading-zero and very long digit strings digit by digit; a leading
///   `-` reads "minus" and `1990-2000` reads "to".
/// * Spoken symbols: `&` `%` `+` `=` `@` `#` `$` `€` `£` `¥` `°` … count their spoken form.
pub fn syllables(text: &str) -> u32 {
    let chars: Vec<char> = text.chars().collect();
    let mut total = 0u32;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            let start = i;
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j].is_ascii_digit() {
                    j += 1;
                } else if matches!(chars[j], '.' | ',' | ':')
                    && chars.get(j + 1).is_some_and(char::is_ascii_digit)
                {
                    j += 2;
                } else {
                    break;
                }
            }
            let run: String = chars[start..j].iter().collect();
            total = total.saturating_add(number_syllables(&run));
            // A leading minus sign ("-5") reads "minus"; a dash between numbers ("1990-2000") reads "to".
            if start >= 1
                && matches!(chars[start - 1], '-' | '−')
                && (start == 1 || !chars[start - 2].is_alphanumeric())
            {
                total = total.saturating_add(2);
            }
            if j + 1 < chars.len() && matches!(chars[j], '-' | '–') && chars[j + 1].is_ascii_digit()
            {
                total = total.saturating_add(1);
            }
            i = j + ordinal_suffix_len(&chars[j..]);
            continue;
        }
        if c.is_alphabetic() {
            let start = i;
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_alphabetic()
                    || is_mark(chars[j])
                    || (is_apostrophe(chars[j])
                        && chars.get(j + 1).is_some_and(|c| c.is_alphabetic())))
            {
                j += 1;
            }
            total = total.saturating_add(letter_run_syllables(&chars[start..j]));
            i = j;
            continue;
        }
        total = total.saturating_add(if c.is_numeric() {
            1
        } else {
            symbol_syllables(c)
        });
        i += 1;
    }
    total
}

/// Syllables of a spoken symbol, 0 for anything that is not read aloud.
fn symbol_syllables(c: char) -> u32 {
    match c {
        '&' | '+' | '@' | '#' | '£' | '¥' | '₩' | '×' => 1,
        '%' | '=' | '$' | '€' | '₹' | '°' | '½' | '¼' | '¾' | '§' => 2,
        '‰' | '÷' => 3,
        _ => 0,
    }
}

/// Length of an ordinal or plural suffix directly after digits (`1st`, `22nd`, `3rd`, `4th`, `1990s`).
fn ordinal_suffix_len(rest: &[char]) -> usize {
    let lower: Vec<char> = rest
        .iter()
        .take(3)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let len = match lower.as_slice() {
        ['s', 't', ..] | ['n', 'd', ..] | ['r', 'd', ..] | ['t', 'h', ..] => 2,
        ['s', ..] | ['º', ..] | ['ª', ..] => 1,
        _ => 0,
    };
    if len > 0 && rest.get(len).is_some_and(|c| c.is_alphabetic()) {
        0
    } else {
        len
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Greek,
    Cyrillic,
    Han,
    Kana,
    Hangul,
    Other,
}

fn script_of(c: char) -> Script {
    let u = c as u32;
    if c.is_ascii_alphabetic()
        || matches!(u, 0x00C0..=0x024F | 0x0250..=0x02AF | 0x1E00..=0x1EFF | 0x2C60..=0x2C7F)
    {
        Script::Latin
    } else if matches!(u, 0x0370..=0x03FF | 0x1F00..=0x1FFF) {
        Script::Greek
    } else if matches!(u, 0x0400..=0x052F | 0x1C80..=0x1C8F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F) {
        Script::Cyrillic
    } else if is_kana(c) || (is_non_starter(c) && !is_han(c)) {
        Script::Kana
    } else if is_cjk_char(c) {
        Script::Han
    } else if is_hangul(c) {
        Script::Hangul
    } else {
        Script::Other
    }
}

/// Syllables of a run of letters (possibly mixing scripts).
fn letter_run_syllables(run: &[char]) -> u32 {
    let mut total = 0u32;
    let mut i = 0;
    while i < run.len() {
        if !run[i].is_alphabetic() {
            i += 1;
            continue;
        }
        let script = script_of(run[i]);
        let mut j = i + 1;
        while j < run.len() && (!run[j].is_alphabetic() || script_of(run[j]) == script) {
            j += 1;
        }
        let part = &run[i..j];
        let letters = part.iter().filter(|c| c.is_alphabetic()).count();
        let count = match script {
            Script::Latin => latin_syllables(part),
            Script::Cyrillic => {
                let vowels = part
                    .iter()
                    .flat_map(|c| c.to_lowercase())
                    .filter(|&c| is_cyrillic_vowel(c))
                    .count();
                to_u32(vowels.max(1))
            }
            Script::Greek => vowel_groups(part, is_greek_vowel).max(1),
            Script::Han | Script::Hangul => to_u32(letters),
            Script::Kana => kana_morae(part),
            Script::Other => to_u32(((letters as f64) / 2.5).round().max(1.0) as usize),
        };
        total = total.saturating_add(count);
        i = j;
    }
    total
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn vowel_groups(part: &[char], is_vowel: impl Fn(char) -> bool) -> u32 {
    let mut count = 0u32;
    let mut prev = false;
    for c in part.iter().flat_map(|c| c.to_lowercase()) {
        let v = is_vowel(c);
        if v && !prev {
            count += 1;
        }
        prev = v;
    }
    count
}

/// Interjections without vowels that are a single syllable.
const VOWELLESS_INTERJECTIONS: &[&str] = &[
    "brr", "grr", "hm", "hmm", "hmmm", "hmph", "mhm", "mm", "mmm", "pfft", "psst", "pst", "sh",
    "shh", "ssh", "tsk", "zzz",
];

fn latin_syllables(part: &[char]) -> u32 {
    let letters: Vec<char> = part.iter().copied().filter(|c| c.is_alphabetic()).collect();
    let n = letters.len();
    if n == 0 {
        return 0;
    }
    if (2..=3).contains(&n) && letters.iter().all(|c| c.is_uppercase()) {
        // Acronym spelled out: "SQL", "AI", "FBI" ("W" is "double-u").
        return letters.iter().map(|&c| if c == 'W' { 3 } else { 1 }).sum();
    }
    let lower: Vec<char> = letters.iter().flat_map(|c| c.to_lowercase()).collect();
    let mut count = 0u32;
    let mut prev = false;
    for (i, &c) in lower.iter().enumerate() {
        // A leading "y" before a vowel is a consonant ("yes").
        let v = is_latin_vowel(c)
            && !(c == 'y' && i == 0 && lower.get(1).is_some_and(|&n| is_latin_vowel(n)));
        if v && !prev {
            count += 1;
        }
        prev = v;
    }
    if count == 0 {
        let word: String = lower.iter().collect();
        return if VOWELLESS_INTERJECTIONS.contains(&word.as_str()) {
            1
        } else {
            to_u32(n.min(4))
        };
    }
    if lower.iter().all(char::is_ascii) && count > 1 {
        count -= english_silent_vowels(&lower);
    }
    count.max(1)
}

/// English endings whose vowel is usually silent: final "e" (not "-le" after a consonant), "-es" and
/// "-ed" after letters that do not need the extra syllable.
fn english_silent_vowels(w: &[char]) -> u32 {
    let n = w.len();
    let v = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
    if n > 2 && w[n - 1] == 'e' && !v(w[n - 2]) {
        let le = w[n - 2] == 'l' && n > 3 && !v(w[n - 3]);
        return u32::from(!le);
    }
    if n > 3 && w[n - 2] == 'e' && !v(w[n - 3]) {
        let c = w[n - 3];
        return match w[n - 1] {
            'd' => u32::from(c != 't' && c != 'd'),
            's' => u32::from(!matches!(c, 's' | 'x' | 'z' | 'c' | 'g' | 'h')),
            _ => 0,
        };
    }
    0
}

/// Lowercase Latin vowels, including accented forms (Vietnamese U+1EA0–U+1EF9 are handled by range).
const LATIN_VOWELS: &str = "aeiouyàáâãäåāăąæèéêëēĕėęěìíîïĩīĭįıòóôõöøōŏőœùúûüũūŭůűųýÿŷơưǎǐǒǔ";

fn is_latin_vowel(c: char) -> bool {
    LATIN_VOWELS.contains(c) || matches!(c as u32, 0x1EA0..=0x1EF9)
}

/// Lowercase Cyrillic vowel letters (each is one syllable).
const CYRILLIC_VOWELS: &str = "аеёиоуыэюяіїєў";

fn is_cyrillic_vowel(c: char) -> bool {
    CYRILLIC_VOWELS.contains(c)
}

/// Lowercase Greek vowels (polytonic forms U+1F00–U+1FFF are handled by range).
const GREEK_VOWELS: &str = "αεηιουωάέήίόύώϊϋΐΰ";

fn is_greek_vowel(c: char) -> bool {
    GREEK_VOWELS.contains(c) || (matches!(c as u32, 0x1F00..=0x1FFF) && !matches!(c, 'ῤ' | 'ῥ'))
}

/// Small kana and sound marks that add no mora (the geminate `っ`/`ッ`/`ｯ` does).
const SMALL_KANA: &str = "ぁぃぅぇぉゃゅょゎゕゖァィゥェォャュョヮヵヶ゛゜ﾞﾟｧｨｩｪｫｬｭｮ";

/// Japanese morae of a kana run.
fn kana_morae(part: &[char]) -> u32 {
    let morae = part
        .iter()
        .filter(|&&c| c.is_alphabetic() && !SMALL_KANA.contains(c))
        .count();
    to_u32(morae.max(1))
}

const DIGIT_SYLLABLES: [u32; 10] = [2, 1, 1, 1, 1, 1, 1, 2, 1, 1]; // zero one two … seven(2) … nine
const TEEN_SYLLABLES: [u32; 10] = [1, 3, 1, 2, 2, 2, 2, 3, 2, 2]; // ten eleven(3) twelve thirteen … seventeen(3) …
const TENS_SYLLABLES: [u32; 10] = [0, 0, 2, 2, 2, 2, 2, 3, 2, 2]; // twenty … seventy(3) …

/// Syllables of a digit run (ASCII digits with optional `.`/`,`/`:` separators between digits), read in
/// English.
fn number_syllables(run: &str) -> u32 {
    if run.contains(':') {
        return run.split(':').map(clock_part_syllables).sum();
    }
    let groups: Vec<&str> = run.split(['.', ',']).collect();
    if groups.len() == 1 {
        return integer_syllables(run);
    }
    let separators: Vec<char> = run.chars().filter(|c| matches!(c, '.' | ',')).collect();
    let uniform = separators.iter().all(|&c| c == separators[0]);
    let grouped =
        uniform && (1..=3).contains(&groups[0].len()) && groups[1..].iter().all(|g| g.len() == 3);
    if grouped {
        // Thousands separators: "1,000,000", "1.000".
        return integer_syllables(&groups.concat());
    }
    // The last separator is the decimal point; earlier ones are thousands separators.
    let (int_groups, frac) = groups.split_at(groups.len() - 1);
    let int_digits = int_groups.concat();
    let int_part = if int_digits.is_empty() {
        0
    } else {
        integer_syllables(&int_digits)
    };
    let frac_part: u32 = frac[0]
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| DIGIT_SYLLABLES[d as usize])
        .sum();
    int_part + 1 + frac_part // "point"
}

/// Hours or minutes of a clock time: "05" reads "oh five", "00" reads "hundred" ("ten hundred") or
/// "o'clock" (2 either way).
fn clock_part_syllables(part: &str) -> u32 {
    match part.as_bytes() {
        [b'0', b'0'] => 2,
        [b'0', d] if d.is_ascii_digit() => 1 + DIGIT_SYLLABLES[usize::from(*d - b'0')],
        _ => integer_syllables(part),
    }
}

/// Syllables of an ASCII digit string read as an English number.
fn integer_syllables(digits: &str) -> u32 {
    let per_digit = || {
        digits
            .chars()
            .filter_map(|c| c.to_digit(10))
            .map(|d| DIGIT_SYLLABLES[d as usize])
            .sum()
    };
    if digits.is_empty() {
        return 0;
    }
    if digits.len() > 12 || (digits.len() > 1 && digits.starts_with('0')) {
        return per_digit();
    }
    let Ok(n) = digits.parse::<u64>() else {
        return per_digit();
    };
    if digits.len() == 4 && ((1100..=1999).contains(&n) || (2010..=2099).contains(&n)) {
        // Years are read in pairs: "nineteen ninety-nine", "twenty twenty-four", "nineteen hundred".
        let (hi, lo) = (n / 100, n % 100);
        let lo_syllables = match lo {
            0 => 2,
            1..=9 => 1 + DIGIT_SYLLABLES[lo as usize],
            _ => below_hundred(lo),
        };
        return below_hundred(hi) + lo_syllables;
    }
    if n == 0 {
        return 2;
    }
    let mut total = 0;
    let mut rest = n;
    let mut scale = 0;
    while rest > 0 {
        let group = rest % 1000;
        if group > 0 {
            total += below_thousand(group) + if scale > 0 { 2 } else { 0 }; // thousand / million / billion
        }
        rest /= 1000;
        scale += 1;
    }
    total
}

fn below_thousand(n: u64) -> u32 {
    let (hundreds, rest) = (n / 100, n % 100);
    let h = if hundreds > 0 {
        DIGIT_SYLLABLES[hundreds as usize] + 2
    } else {
        0
    };
    h + if rest > 0 { below_hundred(rest) } else { 0 }
}

fn below_hundred(n: u64) -> u32 {
    match n {
        0..=9 => DIGIT_SYLLABLES[n as usize],
        10..=19 => TEEN_SYLLABLES[(n - 10) as usize],
        _ => {
            TENS_SYLLABLES[(n / 10) as usize]
                + DIGIT_SYLLABLES[(n % 10) as usize] * u32::from(!n.is_multiple_of(10))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(text: &str) -> Vec<String> {
        tokenize(text).into_iter().map(|t| t.text).collect()
    }

    fn rebuild(text: &str) -> String {
        let mut out = String::new();
        for t in tokenize(text) {
            if t.space_before {
                out.push(' ');
            }
            out.push_str(&t.text);
        }
        out
    }

    #[test]
    fn whitespace_normalization() {
        assert_eq!(normalize_whitespace("  a \n\t b  "), "a b");
        assert_eq!(normalize_whitespace("10\u{a0}km"), "10\u{a0}km");
        assert_eq!(
            normalize_whitespace("soft\u{ad}hyphen\u{feff}"),
            "softhyphen"
        );
        assert_eq!(normalize_whitespace("a\u{3000}b"), "a b");
        assert_eq!(normalize_whitespace(""), "");
        assert_eq!(normalize_whitespace(" \n "), "");
    }

    #[test]
    fn words_keep_attached_punctuation() {
        assert_eq!(texts("Hello, world!"), ["Hello,", "world!"]);
        assert_eq!(
            texts("\"Quoted,\" she said (quietly)."),
            ["\"Quoted,\"", "she", "said", "(quietly)."]
        );
        assert_eq!(
            texts("don't rock’n’roll state-of-the-art"),
            ["don't", "rock’n’roll", "state-of-the-art"]
        );
        assert_eq!(
            texts("It costs $3,000.50 (12.5%)"),
            ["It", "costs", "$3,000.50", "(12.5%)"]
        );
    }

    #[test]
    fn unspaced_dashes_and_ellipses_split_without_space() {
        let tokens = tokenize("wait—what so...then 1990–2000 a--b");
        let t: Vec<(&str, bool)> = tokens
            .iter()
            .map(|t| (t.text.as_str(), t.space_before))
            .collect();
        assert_eq!(
            t,
            [
                ("wait—", false),
                ("what", false),
                ("so...", true),
                ("then", false),
                ("1990–2000", true),
                ("a--", true),
                ("b", false)
            ]
        );
        assert_eq!(rebuild("wait—what so...then"), "wait—what so...then");
    }

    #[test]
    fn standalone_punctuation_merges_into_neighbours() {
        assert_eq!(texts("Wait — what?"), ["Wait —", "what?"]);
        assert_eq!(texts("Quoi ? « Oui » !"), ["Quoi ?", "« Oui » !"]);
        assert_eq!(texts("— Hello"), ["— Hello"]);
        assert_eq!(texts("Great job 🎉"), ["Great", "job 🎉"]);
        assert_eq!(texts("great🎉 day"), ["great🎉", "day"]);
        assert_eq!(texts("..."), ["..."]);
        assert_eq!(texts("Hello « — world"), ["Hello", "« — world"]);
        assert_eq!(rebuild("Quoi ? « Oui » !"), "Quoi ? « Oui » !");
    }

    #[test]
    fn spoken_symbols_are_tokens() {
        assert_eq!(texts("R & D + 5 %"), ["R", "&", "D", "+", "5", "%"]);
        assert_eq!(match_key("&"), "and");
        assert_eq!(match_key("AT&T's"), "atandts");
    }

    #[test]
    fn cjk_is_split_per_character() {
        let tokens = tokenize("你好，世界。「今天」");
        let t: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(t, ["你", "好，", "世", "界。", "「今", "天」"]);
        assert!(tokens.iter().all(|t| !t.space_before));
        assert_eq!(tokens[1].break_after, BreakStrength::Comma);
        assert_eq!(tokens[3].break_after, BreakStrength::Sentence);
    }

    #[test]
    fn japanese_small_kana_attach() {
        assert_eq!(
            texts("きょうはコーヒー"),
            ["きょ", "う", "は", "コー", "ヒー"]
        );
        assert_eq!(texts("東京タワー"), ["東", "京", "タ", "ワー"]);
    }

    #[test]
    fn mixed_cjk_and_latin() {
        let tokens = tokenize("我用AI写代码");
        let t: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(t, ["我", "用", "AI", "写", "代", "码"]);
        assert_eq!(rebuild("我用AI写代码"), "我用AI写代码");
    }

    #[test]
    fn korean_keeps_words() {
        assert_eq!(texts("안녕하세요 여러분"), ["안녕하세요", "여러분"]);
        assert_eq!(tokenize("안녕하세요")[0].syllables, 5);
    }

    #[test]
    fn no_break_space_and_zero_width_space() {
        assert_eq!(texts("10\u{a0}km away"), ["10\u{a0}km", "away"]);
        let tokens = tokenize("ภาษา\u{200B}ไทย");
        let t: Vec<(&str, bool)> = tokens
            .iter()
            .map(|t| (t.text.as_str(), t.space_before))
            .collect();
        assert_eq!(t, [("ภาษา", false), ("ไทย", false)]);
    }

    #[test]
    fn empty_and_whitespace() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("  \n ").is_empty());
    }

    #[test]
    fn audio_tags_are_stripped() {
        assert_eq!(
            display_text("[whispering] Don't tell [laughs]anyone.", None),
            "Don't tell anyone."
        );
        assert_eq!(
            display_text("Hello <break time=\"1.5s\" /> world", None),
            "Hello world"
        );
        assert_eq!(
            display_text(
                "Say <phoneme alphabet=\"ipa\" ph=\"x\">tomato</phoneme>.",
                None
            ),
            "Say tomato."
        );
        assert_eq!(display_text("Hello[pause]world", None), "Hello world");
        assert_eq!(display_text("A[x][y]B", None), "A B");
        assert_eq!(display_text("Pause [[slnc 500]] here", None), "Pause here");
        assert_eq!(display_text("a < b and c > d", None), "a < b and c > d");
        assert_eq!(display_text("x<y and y>z", None), "x<y and y>z");
        assert_eq!(display_text("[] and [unclosed", None), "[] and [unclosed");
        // A caption override is shown verbatim (SDH sound descriptions stay).
        assert_eq!(
            display_text("ignored", Some(" [laughs]  Okay ")),
            "[laughs] Okay"
        );
    }

    #[test]
    fn break_strengths() {
        let b = |t: &str, n: Option<&str>| break_strength(t, n);
        assert_eq!(b("end.", Some("Next")), BreakStrength::Sentence);
        assert_eq!(b("end.\"", Some("Next")), BreakStrength::Sentence);
        assert_eq!(b("really?!", None), BreakStrength::Sentence);
        assert_eq!(b("Dr.", Some("Smith")), BreakStrength::None);
        assert_eq!(b("J.", Some("Doe")), BreakStrength::None);
        assert_eq!(b("e.g.", Some("apples")), BreakStrength::None);
        assert_eq!(b("U.S.", Some("It's")), BreakStrength::Sentence);
        assert_eq!(b("1999.", Some("2000")), BreakStrength::Sentence);
        assert_eq!(b("well…", Some("maybe")), BreakStrength::Clause);
        assert_eq!(b("well...", Some("Maybe")), BreakStrength::Sentence);
        assert_eq!(b("note:", Some("x")), BreakStrength::Clause);
        assert_eq!(b("wait—", Some("what")), BreakStrength::Clause);
        assert_eq!(b("one,", Some("two")), BreakStrength::Comma);
        assert_eq!(b("(aside),", Some("x")), BreakStrength::Comma);
        assert_eq!(b("plain", Some("x")), BreakStrength::None);
        assert_eq!(b("3.14", Some("x")), BreakStrength::None);
    }

    #[test]
    fn english_syllables() {
        let cases = [
            ("a", 1),
            ("the", 1),
            ("make", 1),
            ("table", 2),
            ("people", 2),
            ("makes", 1),
            ("boxes", 2),
            ("jumped", 1),
            ("wanted", 2),
            ("software", 2),
            ("development", 4),
            ("autonomous", 4),
            ("yes", 1),
            ("don't", 1),
            ("you're", 1),
            ("strengths", 1),
            ("state-of-the-art", 4),
            ("SQL", 3),
            ("API", 3),
            ("WWW", 9),
            ("NASA", 2),
            ("km", 2),
            ("hmm", 1),
            ("café", 2),
            ("über", 2),
        ];
        for (word, expected) in cases {
            assert_eq!(syllables(word), expected, "{word}");
        }
    }

    #[test]
    fn number_syllables_read_in_english() {
        let cases = [
            ("0", 2),
            ("7", 2),
            ("11", 3),
            ("21", 3),
            ("100", 3),
            ("1999", 5),
            ("2024", 5),
            ("1900", 4),
            ("1905", 4),
            ("2000", 3),
            ("2,500", 6),
            ("1,000,000", 3),
            ("3.14", 4),
            ("10:30", 3),
            ("9:05", 3),
            ("007", 6),
            ("1st", 1),
            ("1990s", 4),
            ("-5", 3),
            ("1990-2000", 8),
            ("$5", 3),
            ("50%", 4),
        ];
        for (number, expected) in cases {
            assert_eq!(syllables(number), expected, "{number}");
        }
    }

    #[test]
    fn other_scripts_syllables() {
        assert_eq!(syllables("你好"), 2);
        assert_eq!(syllables("きょう"), 2);
        assert_eq!(syllables("コーヒー"), 4);
        assert_eq!(syllables("привет"), 2);
        assert_eq!(syllables("καλημέρα"), 4);
        assert_eq!(syllables("שלום"), 2);
        assert_eq!(syllables("…"), 0);
        assert_eq!(syllables("🎉"), 0);
    }

    #[test]
    fn weights_prefer_longer_words() {
        let short = token_weight("a", syllables("a"));
        let long = token_weight("strengths", syllables("strengths"));
        assert_eq!(short, 1.0);
        assert!(long > 2.0 && long < 2.5, "{long}");
        assert_eq!(token_weight("…", 0), 0.0);
    }

    #[test]
    fn display_len_counts_graphemes() {
        assert_eq!(display_len("e\u{301}"), 1);
        assert_eq!(display_len("👩‍👩‍👧"), 1);
        assert_eq!(display_len("abc"), 3);
    }
}
