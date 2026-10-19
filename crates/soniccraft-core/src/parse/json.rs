//! JSON front end: serde_json → `serde_json::Value`, plus two small hand-written scanners that enforce
//! what serde_json does not, with the same rules as the YAML front end:
//!
//! - a pre-scan (before serde_json allocates anything) limits the document to [`MAX_NODES`] values and a
//!   nesting depth of [`MAX_NESTING_DEPTH`];
//! - a position-tracking scanner over the accepted text rejects duplicate keys and builds the span map
//!   (annotation subtrees `x-…` / `x_…` get no spans: nothing ever reports a position inside them).
//!
//! Comments (`//`, `/* */`, `#`) are not JSON; they get a dedicated `json_syntax` diagnostic.

use std::collections::HashSet;
use std::ops::Range;

use serde_json::Value;

use super::{MAX_NESTING_DEPTH, MAX_NODES, SpanIndex};
use crate::diagnostics::{Diagnostic, codes};
use crate::fields;

/// What kind of syntax error a JSON document has (for the auto-format fallback to YAML).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorKind {
    /// A `//`, `/* */` or `#` comment.
    Comment {
        /// True for `#` comments (YAML has those; `//` and `/*` exist in neither format).
        hash: bool,
    },
    /// A comma before `]` / `}`.
    TrailingComma,
    /// An object key without double quotes (`{version: "1.0"}`, `{'version': …}`): YAML flow style.
    UnquotedKey,
    /// Anything else (including the size and nesting limits and duplicate keys).
    Other,
}

/// A JSON syntax problem: the diagnostic plus its kind.
#[derive(Debug, Clone)]
pub(super) struct JsonError {
    pub diagnostic: Box<Diagnostic>,
    pub kind: ErrorKind,
}

impl JsonError {
    fn other(diagnostic: Diagnostic) -> Self {
        JsonError {
            diagnostic: Box::new(diagnostic),
            kind: ErrorKind::Other,
        }
    }
}

/// Byte length of a leading UTF-8 byte-order mark (ignored by both front ends).
fn bom_len(text: &str) -> usize {
    if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    }
}

/// Parse JSON text (a leading UTF-8 BOM is ignored) into a value and its span map.
pub(super) fn parse(text: &str) -> Result<(Value, SpanIndex), JsonError> {
    let text = &text[bom_len(text)..];
    if let Some(problem) = prescan(text) {
        return Err(JsonError::other(problem));
    }
    let value: Value = serde_json::from_str(text).map_err(|e| serde_error(text, &e))?;
    let scan = Scanner::new(text, Mode::Spans).run();
    if let Some(problem) = scan.problem {
        return Err(JsonError::other(problem));
    }
    Ok((value, scan.spans))
}

/// Byte ranges (in `text`, BOM included) of the values at `values` and of the keys of the entries at
/// `keys`. Values that are objects or lists contribute the ranges of every key and scalar inside them.
pub(super) fn ranges(
    text: &str,
    values: &HashSet<String>,
    keys: &HashSet<String>,
) -> Vec<Range<usize>> {
    let offset = bom_len(text);
    let body = &text[offset..];
    if prescan(body).is_some() || serde_json::from_str::<serde::de::IgnoredAny>(body).is_err() {
        return Vec::new();
    }
    let mut scanner = Scanner::new(body, Mode::Ranges { values, keys });
    scanner.run_ranges();
    scanner
        .ranges
        .into_iter()
        .map(|r| r.start + offset..r.end + offset)
        .collect()
}

/// Would `text` be valid JSON (modulo duplicate keys and limits) once its comments and trailing commas are
/// removed, or does it fail for another reason? Used to tell "JSON with comments" (report the JSON error)
/// from YAML flow style that happens to contain `#` comments (parse as YAML).
pub(super) fn kind_without_comments(text: &str) -> Option<ErrorKind> {
    let cleaned = strip_comments_and_trailing_commas(&text[bom_len(text)..]);
    match serde_json::from_str::<serde::de::IgnoredAny>(&cleaned) {
        Ok(_) => None,
        Err(e) => Some(serde_error(&cleaned, &e).kind),
    }
}

// ---- pre-scan: node budget and depth -------------------------------------------------------------------

/// 1-based line and character column of byte `offset`.
fn position(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = before[line_start..].chars().count() + 1;
    (
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(column).unwrap_or(u32::MAX),
    )
}

/// End of the comment starting at `i` (`//` and `#` run to the end of the line, `/*` to `*/`), if one starts
/// there.
fn comment_end(bytes: &[u8], i: usize) -> Option<usize> {
    match (bytes.get(i), bytes.get(i + 1)) {
        (Some(b'/'), Some(b'/')) | (Some(b'#'), _) => Some(
            bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |p| i + p),
        ),
        (Some(b'/'), Some(b'*')) => Some(
            bytes[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |p| i + 2 + p + 2),
        ),
        _ => None,
    }
}

/// Count values (objects, lists, scalars and keys — the YAML parser's "nodes") and nesting without building
/// anything. Runs on any text; syntax errors are left to serde_json.
fn prescan(text: &str) -> Option<Diagnostic> {
    let bytes = text.as_bytes();
    let mut nodes = 0usize;
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'"' => {
                nodes += 1;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'{' | b'[' => {
                nodes += 1;
                depth += 1;
                i += 1;
                if depth > MAX_NESTING_DEPTH {
                    let (line, column) = position(text, start);
                    return Some(too_deep().at(line, column));
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'/' | b'#' if comment_end(bytes, i).is_some() => {
                i = comment_end(bytes, i).unwrap_or(bytes.len());
            }
            b if b == b'-' || b.is_ascii_alphanumeric() => {
                nodes += 1;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'+' | b'-' | b'.'))
                {
                    i += 1;
                }
            }
            _ => i += 1,
        }
        if nodes > MAX_NODES {
            let (line, column) = position(text, start);
            return Some(
                Diagnostic::error(
                    codes::JSON_SYNTAX,
                    format!("the document has too many values (more than {MAX_NODES} nodes)"),
                )
                .with_hint("Split the composition into several timelines")
                .at(line, column),
            );
        }
    }
    None
}

fn too_deep() -> Diagnostic {
    Diagnostic::error(
        codes::JSON_SYNTAX,
        format!("the document is nested too deeply (more than {MAX_NESTING_DEPTH} levels)"),
    )
    .with_hint("Timelines need only a few levels of nesting")
}

/// `text` with every comment and every trailing comma replaced by spaces (strings are left alone).
fn strip_comments_and_trailing_commas(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();
    let mut i = 0;
    let mut last_comma: Option<usize> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                last_comma = None;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                continue;
            }
            b'/' | b'#' if comment_end(&bytes, i).is_some() => {
                let end = comment_end(&bytes, i).unwrap_or(bytes.len());
                for b in &mut bytes[i..end] {
                    if *b != b'\n' {
                        *b = b' ';
                    }
                }
                i = end;
                continue;
            }
            b',' => last_comma = Some(i),
            b'}' | b']' => {
                if let Some(c) = last_comma.take() {
                    bytes[c] = b' ';
                }
            }
            b if b.is_ascii_whitespace() => {}
            _ => last_comma = None,
        }
        i += 1;
    }
    // Only ASCII bytes outside strings were replaced by ASCII spaces, so the text is still UTF-8.
    String::from_utf8(bytes).unwrap_or_else(|_| text.to_string())
}

// ---- serde_json errors ---------------------------------------------------------------------------------

/// Byte offset of serde_json's 1-based `line` / byte `column` in `text`.
fn byte_offset(text: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let line_len = text[start..].find('\n').unwrap_or(text.len() - start);
    Some(start + column.saturating_sub(1).min(line_len))
}

fn serde_error(text: &str, error: &serde_json::Error) -> JsonError {
    let full = error.to_string();
    // serde_json appends " at line L column C"; positions are reported separately.
    let message = match full.rfind(" at line ") {
        Some(cut) => full[..cut].to_string(),
        None => full,
    };
    let offset = (error.line() > 0)
        .then(|| byte_offset(text, error.line(), error.column()))
        .flatten();
    let at = |offset: usize| text.as_bytes().get(offset).copied();
    let comment = offset.filter(|&o| comment_end(text.as_bytes(), o).is_some());
    let (message, hint, kind) = if let Some(o) = comment {
        (
            "JSON does not allow comments".to_string(),
            "Delete the comment (use an `x-note` key, or write the timeline as YAML where `#` comments work)"
                .to_string(),
            ErrorKind::Comment {
                hash: at(o) == Some(b'#'),
            },
        )
    } else if message.contains("recursion limit") {
        (
            format!("the document is nested too deeply (more than {MAX_NESTING_DEPTH} levels)"),
            "Timelines need only a few levels of nesting".to_string(),
            ErrorKind::Other,
        )
    } else if message.contains("trailing comma") {
        (
            message,
            "Remove the comma after the last element; JSON does not allow trailing commas"
                .to_string(),
            ErrorKind::TrailingComma,
        )
    } else if message.contains("key must be a string") {
        let unquoted = offset
            .and_then(at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'\'' | b'-'));
        (
            message,
            "Quote every object key with double quotes, e.g. \"tracks\": [...]".to_string(),
            if unquoted {
                ErrorKind::UnquotedKey
            } else {
                ErrorKind::Other
            },
        )
    } else if message.contains("EOF") {
        (
            message,
            "The document ends early; check that every `{` and `[` is closed".to_string(),
            ErrorKind::Other,
        )
    } else if message.contains("number out of range") {
        (message, "Use a finite number".to_string(), ErrorKind::Other)
    } else {
        (
            message,
            "Check the JSON syntax at this position: strings use double quotes, elements are separated by commas, \
             and comments are not allowed (use YAML for comments)"
                .to_string(),
            ErrorKind::Other,
        )
    };
    let mut d = Diagnostic::error(codes::JSON_SYNTAX, message).with_hint(hint);
    if error.line() > 0 {
        let line = error.line();
        let column = char_column(text, line, error.column());
        d = d.at(
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column).unwrap_or(u32::MAX),
        );
    }
    JsonError {
        diagnostic: Box::new(d),
        kind,
    }
}

/// Convert serde_json's byte-based column on `line` (1-based) into a 1-based character column.
fn char_column(text: &str, line: usize, byte_column: usize) -> usize {
    let Some(line_text) = text.split('\n').nth(line.saturating_sub(1)) else {
        return byte_column.max(1);
    };
    let mut end = byte_column.min(line_text.len());
    while end > 0 && !line_text.is_char_boundary(end) {
        end -= 1;
    }
    line_text[..end].chars().count().max(1)
}

// ---- scanner: duplicate keys, spans, ranges ------------------------------------------------------------

enum Mode<'w> {
    /// Build the span map (annotation subtrees excluded) and check duplicate keys.
    Spans,
    /// Collect the byte ranges of the wanted values and keys.
    Ranges {
        values: &'w HashSet<String>,
        keys: &'w HashSet<String>,
    },
}

struct ScanResult {
    spans: SpanIndex,
    problem: Option<Diagnostic>,
}

/// A position-tracking JSON scanner over text that serde_json already accepted. It never panics: on any
/// unexpected input it stops and returns what it collected.
struct Scanner<'a, 'w> {
    text: &'a str,
    /// Byte offset of the next character.
    pos: usize,
    line: u32,
    column: u32,
    mode: Mode<'w>,
    spans: SpanIndex,
    ranges: Vec<Range<usize>>,
    problem: Option<Diagnostic>,
}

/// Where the scanner is relative to the requested paths.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Track {
    /// Paths are built and recorded.
    Record,
    /// Inside an annotation subtree (spans mode): no paths.
    Skip,
    /// Inside a wanted value (ranges mode): every key and scalar is a range.
    Collect,
}

impl<'a, 'w> Scanner<'a, 'w> {
    fn new(text: &'a str, mode: Mode<'w>) -> Self {
        Scanner {
            text,
            pos: 0,
            line: 1,
            column: 1,
            mode,
            spans: SpanIndex::new(),
            ranges: Vec::new(),
            problem: None,
        }
    }

    fn run(mut self) -> ScanResult {
        self.skip_ws();
        let _ = self.value(String::new(), Track::Record);
        ScanResult {
            spans: self.spans,
            problem: self.problem,
        }
    }

    fn run_ranges(&mut self) {
        self.skip_ws();
        let _ = self.value(String::new(), Track::Record);
    }

    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.bump();
        }
    }

    fn fail(&mut self, diagnostic: Diagnostic) -> Option<()> {
        if self.problem.is_none() {
            self.problem = Some(diagnostic);
        }
        None
    }

    fn wanted_value(&self, path: &str) -> bool {
        matches!(&self.mode, Mode::Ranges { values, .. } if values.contains(path))
    }

    fn wanted_key(&self, path: &str) -> bool {
        matches!(&self.mode, Mode::Ranges { keys, .. } if keys.contains(path))
    }

    /// Scan one value at `path`.
    fn value(&mut self, path: String, track: Track) -> Option<()> {
        let start = self.pos;
        let track = if track == Track::Record && self.wanted_value(&path) {
            Track::Collect
        } else {
            track
        };
        if track == Track::Record && matches!(self.mode, Mode::Spans) {
            self.spans.insert(path.clone(), self.line, self.column);
        }
        match self.peek()? {
            '{' => self.object(path, track),
            '[' => self.array(path, track),
            '"' => {
                self.string()?;
                if track == Track::Collect {
                    self.ranges.push(start..self.pos);
                }
                Some(())
            }
            _ => {
                // number, true, false, null
                while let Some(c) = self.peek() {
                    if matches!(c, ',' | '}' | ']' | ' ' | '\t' | '\n' | '\r') {
                        break;
                    }
                    self.bump();
                }
                if track == Track::Collect {
                    self.ranges.push(start..self.pos);
                }
                Some(())
            }
        }
    }

    fn object(&mut self, path: String, track: Track) -> Option<()> {
        self.bump(); // {
        self.skip_ws();
        let mut seen = HashSet::new();
        if self.peek()? == '}' {
            self.bump();
            return Some(());
        }
        loop {
            self.skip_ws();
            let (key_line, key_column, key_start) = (self.line, self.column, self.pos);
            let key = self.string()?;
            let key_range = key_start..self.pos;
            let child = if track == Track::Skip {
                String::new()
            } else if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if !seen.insert(key.clone()) {
                let shown = if track == Track::Skip {
                    key.clone()
                } else {
                    child.clone()
                };
                return self.fail(
                    Diagnostic::error(codes::JSON_SYNTAX, format!("duplicate key `{key}` in the same object"))
                        .with_path(shown)
                        .with_hint(format!(
                            "Each key may appear only once per object; remove or merge the repeated `{key}` entry"
                        ))
                        .at(key_line, key_column),
                );
            }
            // An annotation entry keeps its own key and value positions (secret diagnostics point at it),
            // but nothing inside it is indexed.
            let annotation = track == Track::Record
                && matches!(self.mode, Mode::Spans)
                && fields::is_annotation(&key);
            match track {
                Track::Record if matches!(self.mode, Mode::Spans) => {
                    self.spans.insert_key(child.clone(), key_line, key_column);
                }
                Track::Record if self.wanted_key(&child) => self.ranges.push(key_range),
                Track::Collect => self.ranges.push(key_range),
                _ => {}
            }
            self.skip_ws();
            if self.bump()? != ':' {
                return None;
            }
            self.skip_ws();
            if annotation {
                self.spans.insert(child, self.line, self.column);
                self.value(String::new(), Track::Skip)?;
            } else {
                self.value(child, track)?;
            }
            self.skip_ws();
            match self.bump()? {
                ',' => continue,
                '}' => return Some(()),
                _ => return None,
            }
        }
    }

    fn array(&mut self, path: String, track: Track) -> Option<()> {
        self.bump(); // [
        self.skip_ws();
        if self.peek()? == ']' {
            self.bump();
            return Some(());
        }
        let mut index = 0usize;
        loop {
            self.skip_ws();
            let child = if track == Track::Skip {
                String::new()
            } else {
                format!("{path}[{index}]")
            };
            self.value(child, track)?;
            index += 1;
            self.skip_ws();
            match self.bump()? {
                ',' => continue,
                ']' => return Some(()),
                _ => return None,
            }
        }
    }

    /// Scan a string literal and return its decoded content.
    fn string(&mut self) -> Option<String> {
        if self.bump()? != '"' {
            return None;
        }
        let mut out = String::new();
        loop {
            match self.bump()? {
                '"' => return Some(out),
                '\\' => match self.bump()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let high = self.hex4()?;
                        if (0xD800..0xDC00).contains(&high) {
                            // Surrogate pair: expect `\uDC00..DFFF`.
                            if self.bump()? != '\\' || self.bump()? != 'u' {
                                out.push(char::REPLACEMENT_CHARACTER);
                                continue;
                            }
                            let low = self.hex4()?;
                            let code = 0x10000
                                + ((high - 0xD800) << 10)
                                + (low.wrapping_sub(0xDC00) & 0x3FF);
                            out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
                        } else {
                            out.push(char::from_u32(high).unwrap_or(char::REPLACEMENT_CHARACTER));
                        }
                    }
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            v = v * 16 + self.bump()?.to_digit(16)?;
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> JsonError {
        parse(src).unwrap_err()
    }

    #[test]
    fn spans_and_keys() {
        let src = "{\n  \"version\": \"1.0\",\n  \"tracks\": [\n    {\"id\": \"a\", \"type\": \"speech\"}\n  ]\n}\n";
        let (_, s) = parse(src).unwrap();
        assert_eq!(s.get("version"), Some((2, 14)));
        assert_eq!(s.key("version"), Some((2, 3)));
        assert_eq!(s.get("tracks[0]"), Some((4, 5)));
        assert_eq!(s.get("tracks[0].type"), Some((4, 25)));
        assert_eq!(s.key("tracks[0].type"), Some((4, 17)));
    }

    #[test]
    fn escaped_keys_and_unicode_columns() {
        let src = "{\"t\\u00e9xt\": \"é\", \"b\": 1}";
        let (_, s) = parse(src).unwrap();
        assert_eq!(s.key("téxt"), Some((1, 2)));
        assert_eq!(s.get("b"), Some((1, 25)));
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let d = err("{\"a\": 1,\n \"a\": 2}").diagnostic;
        assert_eq!(d.code, "json_syntax");
        assert!(d.message.contains("duplicate key `a`"));
        assert_eq!((d.line, d.column), (Some(2), Some(2)));
        // Also inside annotations (YAML rejects duplicate keys everywhere too).
        let d = err("{\"x-note\": {\"k\": 1, \"k\": 2}}").diagnostic;
        assert!(d.message.contains("duplicate key `k`"), "{}", d.message);
    }

    #[test]
    fn syntax_errors_have_positions() {
        let d = err("{\n  \"a\": 1,\n  \"b\": }\n").diagnostic;
        assert_eq!(d.code, "json_syntax");
        assert_eq!(d.line, Some(3));
        assert!(!d.message.contains(" at line "), "{}", d.message);
    }

    #[test]
    fn depth_limit() {
        let src = format!("{}{}", "[".repeat(65), "]".repeat(65));
        let d = err(&src).diagnostic;
        assert!(d.message.contains("nested too deeply"), "{}", d.message);
        assert_eq!((d.line, d.column), (Some(1), Some(65)));
        let ok = format!("{}{}", "[".repeat(64), "]".repeat(64));
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn node_budget_matches_yaml() {
        // 1 root object + 1 key + 1 array + n elements.
        let doc = |n: usize| format!("{{\"x-big\": [{}]}}", vec!["0"; n].join(","));
        assert!(parse(&doc(MAX_NODES - 3)).is_ok());
        let e = err(&doc(MAX_NODES - 2));
        assert_eq!(e.kind, ErrorKind::Other);
        assert!(
            e.diagnostic
                .message
                .contains("too many values (more than 250000 nodes)"),
            "{}",
            e.diagnostic.message
        );
        assert!(e.diagnostic.line.is_some());
        // Keys count like YAML keys; strings with structural characters are one node.
        let keys = format!(
            "{{{}}}",
            (0..MAX_NODES / 2)
                .map(|i| format!("\"k{i}\": \"[{{,}}]\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse(&keys).is_err());
    }

    #[test]
    fn annotation_subtrees_get_no_spans() {
        let (_, s) =
            parse("{\"a\": 1, \"x-big\": [1, {\"b\": 2}], \"x_n\": {\"c\": [3]}, \"d\": [4]}")
                .unwrap();
        assert!(s.get("a").is_some() && s.get("d[0]").is_some());
        // The annotation entries themselves are positioned, their contents are not.
        assert_eq!(
            (s.key("x-big"), s.get("x-big")),
            (Some((1, 10)), Some((1, 19)))
        );
        assert!(s.get("x-big[0]").is_none() && s.get("x-big[1].b").is_none());
        assert!(s.get("x_n.c").is_none() && s.get("x_n.c[0]").is_none());
        assert_eq!(s.len(), 10, "root, a, x-big, x_n, d (each + key), d[0]");
    }

    #[test]
    fn comments_get_their_own_diagnostic() {
        for (src, pos) in [
            (
                "{\n  \"version\": \"1.0\",\n  // a comment\n  \"tracks\": []\n}",
                (3, 3),
            ),
            (
                "{\n  \"version\": \"1.0\", // comment\n  \"tracks\": []\n}",
                (2, 21),
            ),
            ("// head\n{\"a\": 1}", (1, 1)),
            ("{\"a\": /* c */ 1}", (1, 7)),
            ("{\"a\": 1 # c\n}", (1, 9)),
            ("{\"a\": 1} // trailing", (1, 10)),
        ] {
            let e = err(src);
            assert!(matches!(e.kind, ErrorKind::Comment { .. }), "{src}");
            let d = e.diagnostic;
            assert_eq!(d.message, "JSON does not allow comments", "{src}");
            assert_eq!(
                d.hint.as_deref(),
                Some(
                    "Delete the comment (use an `x-note` key, or write the timeline as YAML where `#` comments \
                     work)"
                )
            );
            assert_eq!((d.line, d.column), (Some(pos.0), Some(pos.1)), "{src}");
        }
        // Comment markers inside strings are text.
        assert!(parse("{\"url\": \"http://x/#y\", \"t\": \"/* no */\"}").is_ok());
    }

    #[test]
    fn error_kinds_for_the_yaml_fallback() {
        assert_eq!(err("{\"a\": 1,}").kind, ErrorKind::TrailingComma);
        assert_eq!(err("{version: \"1.0\"}").kind, ErrorKind::UnquotedKey);
        assert_eq!(err("{'version': '1.0'}").kind, ErrorKind::UnquotedKey);
        assert_eq!(err("{\"a\": yes}").kind, ErrorKind::Other);
        assert_eq!(
            err("{\"a\": 1 # c\n}").kind,
            ErrorKind::Comment { hash: true }
        );
        // What remains once comments and trailing commas are gone.
        assert_eq!(kind_without_comments("{\"a\": [1,], # c\n}"), None);
        assert_eq!(
            kind_without_comments("{ # c\n version: 1}"),
            Some(ErrorKind::UnquotedKey)
        );
        assert_eq!(kind_without_comments("{\"a\": \"#,]\",}"), None);
    }

    #[test]
    fn bom_is_ignored() {
        let (v, s) = parse("\u{feff}{\"a\": 1}").unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(s.key("a"), Some((1, 2)));
    }

    #[test]
    fn byte_ranges_of_values_and_keys() {
        let src = "\u{feff}{\"a\": \"s3cr\\u0065t\", \"x-n\": {\"k\": [1, \"v\"]}, \"b\": 2}";
        let values: HashSet<String> = ["a".to_string(), "x-n".to_string()].into();
        let keys: HashSet<String> = ["b".to_string()].into();
        let r = ranges(src, &values, &keys);
        let texts: Vec<&str> = r.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(texts, ["\"s3cr\\u0065t\"", "\"k\"", "1", "\"v\"", "\"b\""]);
    }
}
