//! Parsing: YAML/JSON text → raw [`Timeline`] + diagnostics + source spans.
//!
//! Pipeline:
//!
//! 1. Limits: documents larger than [`MAX_DOCUMENT_BYTES`] are rejected.
//! 2. Syntax: YAML through serde-saphyr (YAML 1.2 booleans only, duplicate keys rejected, nesting depth
//!    ≤ [`MAX_NESTING_DEPTH`], at most [`MAX_NODES`] values, alias-bomb budget, single document) or JSON
//!    through serde_json (plus scanners that enforce the same depth, value budget and duplicate-key rules,
//!    and a dedicated diagnostic for comments). Failures are `yaml_syntax` / `json_syntax` diagnostics with
//!    line and column; nothing else is reported then.
//! 3. Spans: a [`SpanIndex`] maps every path (`tracks[2].sequence[0].text`) to the position of its value
//!    and `<path>#key` to the position of its key.
//! 4. Secrets: keys like `api_key` and values like `sk-…` are `secret_in_timeline` errors (see
//!    [`secret_spans`] for masking them in source snippets).
//! 5. Structure: the value is deserialized into [`Timeline`] element by element, so one bad element
//!    never hides problems elsewhere (`invalid_type`, `unknown_field` with did-you-mean, synonym and unit
//!    hints). Values whose mistake has one mechanical fix (`Speech` → `speech`, `"350MS"` → `"350ms"`, a
//!    positive loudness target, a platform name as loudness preset) are `invalid_value` errors here, and
//!    the converted value is what later stages see.
//! 6. YAML only: a plain `text`/`caption` scalar cut short by a ` #` comment is a `comment_in_text`
//!    warning.
//!
//! All other semantic rules (required fields, ranges, references, …) are checked by [`crate::lower`].

mod decode;
mod json;
pub(crate) mod secrets;
mod yaml;

use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, codes};
use crate::model::Timeline;

pub use secrets::secret_spans;

/// Largest accepted document, in bytes (5 MiB).
pub const MAX_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;

/// Deepest accepted nesting of mappings and lists (the root mapping is level 1).
pub const MAX_NESTING_DEPTH: usize = 64;

/// Most values (mappings, lists, scalars and keys) a document may contain: serde-saphyr's default node
/// budget for YAML, enforced for JSON too so both formats accept the same documents.
pub const MAX_NODES: usize = 250_000;

/// Input document format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// YAML 1.2 (JSON documents are valid YAML too, but are better served by [`SourceFormat::Json`]).
    Yaml,
    /// JSON (RFC 8259).
    Json,
    /// JSON if the first non-whitespace character is `{`, else YAML. A document that looks like JSON but is
    /// YAML flow style (unquoted keys, `{version: "1.0", …}`) is read as YAML; one that is JSON except for
    /// comments or trailing commas gets the JSON diagnostic.
    Auto,
}

impl SourceFormat {
    /// Pick a format from a file extension (`.json` → Json, `.yaml`/`.yml` → Yaml, else Auto).
    pub fn from_extension(ext: Option<&str>) -> Self {
        match ext.map(|e| e.to_ascii_lowercase()) {
            Some(e) if e == "json" => SourceFormat::Json,
            Some(e) if e == "yaml" || e == "yml" => SourceFormat::Yaml,
            _ => SourceFormat::Auto,
        }
    }

    /// Resolve [`SourceFormat::Auto`] for `text` (a leading byte-order mark is ignored); other formats are
    /// returned unchanged.
    pub fn resolve(self, text: &str) -> SourceFormat {
        match self {
            SourceFormat::Auto => {
                let first = text
                    .trim_start_matches('\u{feff}')
                    .trim_start()
                    .chars()
                    .next();
                if first == Some('{') {
                    SourceFormat::Json
                } else {
                    SourceFormat::Yaml
                }
            }
            other => other,
        }
    }

    /// The syntax-error diagnostic code for this format (`yaml_syntax` / `json_syntax`).
    fn syntax_code(self) -> &'static str {
        match self {
            SourceFormat::Json => codes::JSON_SYNTAX,
            SourceFormat::Yaml | SourceFormat::Auto => codes::YAML_SYNTAX,
        }
    }
}

/// Map from diagnostic paths (`tracks[2].sequence[0].text`) to 1-based `(line, column)` of the value
/// (or of the key, for missing/unknown keys) in the source document.
///
/// Columns count characters. Value positions are stored under the path itself; key positions under
/// `<path>#key` (see [`SpanIndex::key`]). The root value is stored under the empty path.
#[derive(Debug, Clone, Default)]
pub struct SpanIndex {
    map: HashMap<String, (u32, u32)>,
}

/// Suffix of key-position entries in a [`SpanIndex`].
const KEY_SUFFIX: &str = "#key";

impl SpanIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the position of the value at `path`.
    pub fn insert(&mut self, path: impl Into<String>, line: u32, column: u32) {
        self.map.insert(path.into(), (line, column));
    }

    /// Record the position of the key of the mapping entry at `path`.
    pub fn insert_key(&mut self, path: impl Into<String>, line: u32, column: u32) {
        let mut path = path.into();
        path.push_str(KEY_SUFFIX);
        self.map.insert(path, (line, column));
    }

    /// Exact lookup.
    pub fn get(&self, path: &str) -> Option<(u32, u32)> {
        self.map.get(path).copied()
    }

    /// Exact lookup of the key position of the mapping entry at `path`.
    pub fn key(&self, path: &str) -> Option<(u32, u32)> {
        self.map.get(&format!("{path}{KEY_SUFFIX}")).copied()
    }

    /// Lookup of `path` or its longest known ancestor (`a.b[1].c` → `a.b[1]` → `a.b` → `a` → the root).
    pub fn locate(&self, path: &str) -> Option<(u32, u32)> {
        let mut p = path;
        loop {
            if let Some(pos) = self.get(p) {
                return Some(pos);
            }
            match p.rfind(['.', '[']) {
                Some(cut) if cut > 0 => p = &p[..cut],
                _ => return self.get(""),
            }
        }
    }

    /// Fill in `line`/`column` on a diagnostic that has a path but no position. For `unknown_field` and
    /// `field_not_applicable` the key position is preferred (the key itself is the problem).
    pub fn attach(&self, diagnostic: &mut Diagnostic) {
        if diagnostic.line.is_some() {
            return;
        }
        let Some(path) = &diagnostic.path else { return };
        let prefers_key = matches!(
            diagnostic.code.as_str(),
            codes::UNKNOWN_FIELD | codes::FIELD_NOT_APPLICABLE
        );
        let position = if prefers_key {
            self.key(path).or_else(|| self.locate(path))
        } else {
            self.locate(path)
        };
        if let Some((line, column)) = position {
            diagnostic.line = Some(line);
            diagnostic.column = Some(column);
        }
    }

    /// Number of entries (values and keys).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when no positions are known (e.g. the document did not parse).
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Result of [`parse_timeline`].
#[derive(Debug, Clone)]
pub struct Parsed {
    /// The raw AST, if the document was syntactically valid and structurally deserializable (elements that
    /// failed to deserialize are dropped and reported).
    pub timeline: Option<Timeline>,
    /// Parse diagnostics (syntax, `invalid_type`, `unknown_field`, `secret_in_timeline`, `invalid_value`
    /// for values with one mechanical fix, `comment_in_text`), with positions.
    pub diagnostics: Vec<Diagnostic>,
    /// Source positions of every path.
    pub spans: SpanIndex,
    /// The document as a generic JSON value (when syntactically valid).
    pub value: Option<serde_json::Value>,
    /// Paths of values that failed to deserialize and were dropped (or replaced by an empty placeholder)
    /// during error recovery. Each one has an `invalid_type` (or `invalid_value`) diagnostic; diagnostics of
    /// later stages at or below these paths are consequences of the same problem (see
    /// [`Parsed::is_dropped`]).
    pub dropped: Vec<String>,
}

impl Parsed {
    /// True when `path` is at or below a value that was dropped during error recovery.
    pub fn is_dropped(&self, path: &str) -> bool {
        self.dropped.iter().any(|d| {
            path == d
                || path
                    .strip_prefix(d.as_str())
                    .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
        })
    }
}

/// What the syntax stage produced.
struct FrontEnd {
    value: serde_json::Value,
    spans: SpanIndex,
    /// Plain YAML scalars cut short by a ` #` comment.
    comments: Vec<yaml::TrailingComment>,
    /// The format that was actually used.
    format: SourceFormat,
}

/// Syntax stage: text → generic value + spans, with the auto-format rules of [`SourceFormat::Auto`].
fn front_end(text: &str, format: SourceFormat) -> Result<FrontEnd, Box<Diagnostic>> {
    let auto = format == SourceFormat::Auto;
    let yaml = |text: &str| -> Result<FrontEnd, Box<Diagnostic>> {
        // Spans are built only after serde-saphyr accepted the text (alias-bomb safety).
        let value = yaml::parse(text)?;
        let (spans, comments) = yaml::index(text);
        Ok(FrontEnd {
            value,
            spans,
            comments,
            format: SourceFormat::Yaml,
        })
    };
    match format.resolve(text) {
        SourceFormat::Json => match json::parse(text) {
            Ok((value, spans)) => Ok(FrontEnd {
                value,
                spans,
                comments: Vec::new(),
                format: SourceFormat::Json,
            }),
            Err(json_error) => {
                // YAML flow style is JSON-like; fall back only for what it is meant to accept (unquoted keys,
                // possibly with `#` comments or trailing commas). JSON with comments stays JSON.
                let flow_style = auto
                    && match json_error.kind {
                        json::ErrorKind::UnquotedKey => true,
                        json::ErrorKind::Comment { hash: true }
                        | json::ErrorKind::TrailingComma => {
                            json::kind_without_comments(text) == Some(json::ErrorKind::UnquotedKey)
                        }
                        json::ErrorKind::Comment { hash: false } | json::ErrorKind::Other => false,
                    };
                match flow_style.then(|| yaml(text)) {
                    Some(Ok(front)) => Ok(front),
                    _ => Err(json_error.diagnostic),
                }
            }
        },
        SourceFormat::Yaml | SourceFormat::Auto => yaml(text),
    }
}

/// The document as a generic value and the format it was read as (None when it does not parse).
pub(crate) fn document_value(
    text: &str,
    format: SourceFormat,
) -> Option<(serde_json::Value, SourceFormat)> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return None;
    }
    front_end(text, format).ok().map(|f| (f.value, f.format))
}

/// Parse a timeline document.
///
/// With [`SourceFormat::Auto`], a document that looks like JSON but uses YAML flow style (unquoted keys,
/// e.g. `{version: "1.0", tracks: [...]}`) is read as YAML (JSON is a subset of YAML 1.2); a document that is
/// JSON except for comments or trailing commas gets the JSON diagnostic, as does one that is neither.
pub fn parse_timeline(text: &str, format: SourceFormat) -> Parsed {
    let mut parsed = Parsed {
        timeline: None,
        diagnostics: Vec::new(),
        spans: SpanIndex::new(),
        value: None,
        dropped: Vec::new(),
    };

    if text.len() > MAX_DOCUMENT_BYTES {
        parsed.diagnostics.push(
            Diagnostic::error(
                format.resolve(text).syntax_code(),
                format!(
                    "the document is {:.1} MB; the maximum is {} MB",
                    text.len() as f64 / (1024.0 * 1024.0),
                    MAX_DOCUMENT_BYTES / (1024 * 1024)
                ),
            )
            .with_hint("Split the composition into several timelines, or shorten very long texts")
            .at(1, 1),
        );
        return parsed;
    }

    let front = match front_end(text, format) {
        Ok(front) => front,
        Err(d) => {
            parsed.diagnostics.push(*d);
            return parsed;
        }
    };
    let FrontEnd {
        value,
        spans,
        comments,
        ..
    } = front;
    parsed.spans = spans;

    let serde_json::Value::Object(root) = &value else {
        let got = match &value {
            serde_json::Value::Null => "the document is empty",
            serde_json::Value::Array(_) => "the document is a list",
            _ => "the document is a single value",
        };
        parsed.diagnostics.push(
            Diagnostic::error(
                codes::INVALID_TYPE,
                format!("a timeline must be a mapping with `version` and `tracks`, but {got}"),
            )
            .with_hint(
                "Start from a minimal timeline:\nversion: \"1.0\"\ntracks:\n  - id: narration\n    type: speech\n    \
                 sequence:\n      - text: \"Hello!\"\n        provider: mock",
            )
            .at(1, 1),
        );
        parsed.value = Some(value);
        return parsed;
    };

    let secret_scan = secrets::scan(&value);
    let decoded = decode::decode(root, &secret_scan.key_paths);
    parsed.diagnostics.extend(secret_scan.diagnostics);
    parsed.diagnostics.extend(decoded.diagnostics);
    parsed
        .diagnostics
        .extend(comment_warnings(&decoded.timeline, &comments));
    // Positions first: redaction may rewrite paths that contain a secret-shaped key.
    for d in &mut parsed.diagnostics {
        parsed.spans.attach(d);
    }
    secrets::redact(&mut parsed.diagnostics, &secret_scan.values);
    parsed.timeline = Some(decoded.timeline);
    parsed.dropped = decoded.dropped;
    parsed.value = Some(value);
    parsed
}

/// Longest cue text echoed in a `comment_in_text` hint.
const MAX_ECHOED_TEXT: usize = 200;

/// `comment_in_text` warnings for plain `text` / `caption` scalars of cues that a ` #` cut short.
fn comment_warnings(timeline: &Timeline, comments: &[yaml::TrailingComment]) -> Vec<Diagnostic> {
    comments
        .iter()
        .filter_map(|c| {
            let (track, item, field) = cue_field(&c.path)?;
            let label = cue_label(timeline, track, item).unwrap_or_else(|| {
                c.path
                    .rsplit_once('.')
                    .map_or(c.path.clone(), |(item, _)| item.to_string())
            });
            let comment = secrets::mask_tokens(&c.comment);
            let intended = secrets::mask_tokens(&format!("{} {}", c.value, c.comment));
            let what = if field == "text" { "text" } else { "caption" };
            let hint = if intended.chars().count() <= MAX_ECHOED_TEXT {
                let quoted = intended.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    "Quote the {what}: {field}: \"{quoted}\" (if `{}` is meant as a comment, put it on its own line)",
                    comment.chars().take(40).collect::<String>()
                )
            } else {
                format!(
                    "Quote the {what}: wrap the whole value of `{field}` in double quotes; an unquoted ` #` starts a \
                     YAML comment"
                )
            };
            Some(
                Diagnostic::warning(
                    codes::COMMENT_IN_TEXT,
                    format!("the {what} of {label} stops at ` #`: YAML read `{comment}` as a comment"),
                )
                .with_path(c.path.clone())
                .with_hint(hint)
                .at(c.line, c.column),
            )
        })
        .collect()
}

/// `(track, item, "text" | "caption")` for paths `tracks[t].sequence[i].text|caption`.
fn cue_field(path: &str) -> Option<(usize, usize, &str)> {
    let rest = path.strip_prefix("tracks[")?;
    let (track, rest) = rest.split_once("].sequence[")?;
    let (item, field) = rest.split_once("].")?;
    let field = match field {
        "text" | "caption" => field,
        _ => return None,
    };
    Some((track.parse().ok()?, item.parse().ok()?, field))
}

/// "cue `tip`" for the cue at `tracks[track].sequence[item]`: its explicit id, else its automatic id
/// (`<track id>.cue<n>`, n counting the cues of the track).
fn cue_label(timeline: &Timeline, track: usize, item: usize) -> Option<String> {
    let t = timeline.tracks.get(track)?;
    let items = t.sequence.as_ref()?;
    let cue = items.get(item)?;
    if let Some(id) = cue.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        return Some(format!("cue `{id}`"));
    }
    let track_id = t.id.as_deref().map(str::trim).filter(|id| !id.is_empty())?;
    let n = items[..=item]
        .iter()
        .filter(|i| !((i.pause_duration.is_some() || i.pause.is_some()) && i.text.is_none()))
        .count();
    Some(format!("cue `{track_id}.cue{n}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detection() {
        assert_eq!(
            SourceFormat::Auto.resolve("  {\"a\": 1}"),
            SourceFormat::Json
        );
        assert_eq!(SourceFormat::Auto.resolve("\u{feff}{}"), SourceFormat::Json);
        assert_eq!(SourceFormat::Auto.resolve("a: 1"), SourceFormat::Yaml);
        assert_eq!(SourceFormat::Yaml.resolve("{}"), SourceFormat::Yaml);
        assert_eq!(
            SourceFormat::from_extension(Some("JSON")),
            SourceFormat::Json
        );
        assert_eq!(
            SourceFormat::from_extension(Some("yml")),
            SourceFormat::Yaml
        );
        assert_eq!(SourceFormat::from_extension(None), SourceFormat::Auto);
    }

    #[test]
    fn locate_falls_back_to_ancestors_and_root() {
        let mut s = SpanIndex::new();
        s.insert("", 1, 1);
        s.insert("tracks", 2, 1);
        s.insert("tracks[0]", 3, 5);
        s.insert_key("tracks[0].foo", 4, 5);
        assert_eq!(s.locate("tracks[0].bar.baz"), Some((3, 5)));
        assert_eq!(s.locate("version"), Some((1, 1)));
        let mut d = Diagnostic::error(codes::UNKNOWN_FIELD, "x").with_path("tracks[0].foo");
        s.attach(&mut d);
        assert_eq!((d.line, d.column), (Some(4), Some(5)));
        let mut d = Diagnostic::error(codes::INVALID_VALUE, "x").with_path("tracks[0].foo");
        s.attach(&mut d);
        assert_eq!((d.line, d.column), (Some(3), Some(5)));
    }

    #[test]
    fn non_mapping_roots() {
        for (text, needle) in [
            ("", "empty"),
            ("- a\n- b\n", "a list"),
            ("42", "single value"),
        ] {
            let p = parse_timeline(text, SourceFormat::Yaml);
            assert!(p.timeline.is_none());
            assert_eq!(p.diagnostics.len(), 1);
            assert!(
                p.diagnostics[0].message.contains(needle),
                "{}",
                p.diagnostics[0].message
            );
        }
    }

    #[test]
    fn auto_falls_back_to_yaml_for_flow_mappings() {
        let flow = "{version: \"1.0\", tracks: []}";
        let p = parse_timeline(flow, SourceFormat::Auto);
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        assert_eq!(
            p.value.as_ref().map(|v| v["version"].clone()),
            Some(serde_json::json!("1.0"))
        );
        // Explicit JSON stays strict.
        let p = parse_timeline(flow, SourceFormat::Json);
        assert_eq!(p.diagnostics[0].code, "json_syntax");
        // Broken JSON that is not YAML either reports the JSON error.
        let p = parse_timeline("{\"version\": \"1.0\" // comment\n}", SourceFormat::Auto);
        assert_eq!(p.diagnostics[0].code, "json_syntax");
    }

    #[test]
    fn oversized_documents_are_rejected() {
        let text = format!("title: \"{}\"\n", "a".repeat(MAX_DOCUMENT_BYTES));
        let p = parse_timeline(&text, SourceFormat::Yaml);
        assert_eq!(p.diagnostics[0].code, "yaml_syntax");
        assert!(p.diagnostics[0].message.contains("maximum is 5 MB"));
    }

    #[test]
    fn dropped_paths() {
        let p = Parsed {
            timeline: None,
            diagnostics: Vec::new(),
            spans: SpanIndex::new(),
            value: None,
            dropped: vec!["tracks[1]".into(), "tracks[0].volume".into()],
        };
        assert!(p.is_dropped("tracks[1]"));
        assert!(p.is_dropped("tracks[1].id"));
        assert!(p.is_dropped("tracks[1].sequence[0]"));
        assert!(!p.is_dropped("tracks[10]"));
        assert!(!p.is_dropped("tracks[0].volume_x"));
        assert!(p.is_dropped("tracks[0].volume"));
    }
}
