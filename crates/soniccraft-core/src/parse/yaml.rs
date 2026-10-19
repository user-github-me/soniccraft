//! YAML front end: serde-saphyr → `serde_json::Value`, error mapping, and the source span map built from
//! granit-parser events (the parser serde-saphyr itself uses, re-exported as `serde_saphyr::granit_parser`).
//! The same event walk finds plain scalars cut short by a ` #` comment and the byte ranges of values for
//! [`super::secret_spans`].

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use serde_json::Value;
use serde_saphyr::budget::BudgetBreach;
use serde_saphyr::granit_parser::{Event, Marker, Parser, ScalarStyle, Span};
use serde_saphyr::{
    Error as YamlError, MessageFormatter, NonFiniteFloatPolicy, UserMessageFormatter,
};

use super::{MAX_NESTING_DEPTH, SpanIndex};
use crate::diagnostics::{Diagnostic, codes};

/// Parse YAML text into a generic value.
///
/// Options: YAML 1.1 boolean words (`yes`, `no`, `on`, `off`) stay strings (`strict_booleans`),
/// duplicate keys are rejected, the default budget applies (nesting depth 64, 250 000 nodes, alias-bomb
/// limits), multiple documents are rejected, and non-finite floats (`.inf`, `.nan`, `1e999`) become
/// strings so they surface as precise type errors at their path instead of aborting the parse.
pub(super) fn parse(text: &str) -> Result<Value, Box<Diagnostic>> {
    let options = serde_saphyr::options! {
        strict_booleans: true,
        with_snippet: false,
        non_finite_float_policy: NonFiniteFloatPolicy::AsString,
    };
    serde_saphyr::from_str_with_options::<Value>(text, options)
        .map_err(|e| Box::new(error_to_diagnostic(&e)))
}

/// Map a serde-saphyr error to a `yaml_syntax` diagnostic with line/column and a hint.
fn error_to_diagnostic(error: &YamlError) -> Diagnostic {
    let inner = innermost(error);
    let (message, hint) = describe(inner);
    let mut d = Diagnostic::error(codes::YAML_SYNTAX, message).with_hint(hint);
    if let Some(loc) = error.location() {
        let (line, column) = (loc.line(), loc.column());
        if line > 0 {
            d = d.at(clamp_u32(line), clamp_u32(column.max(1)));
        }
    }
    d
}

fn innermost(error: &YamlError) -> &YamlError {
    match error.without_snippet() {
        YamlError::AliasError { error, .. } => innermost(error),
        other => other,
    }
}

fn clamp_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn describe(error: &YamlError) -> (String, String) {
    match error {
        YamlError::DuplicateMappingKey { key, .. } => {
            let key = key.as_deref().map(|k| format!(" `{}`", sanitize(k))).unwrap_or_default();
            (
                format!("duplicate key{key} in the same mapping"),
                format!("Each key may appear only once per mapping; remove or merge the repeated{key} entry"),
            )
        }
        YamlError::MultipleDocuments { .. } => (
            "the file contains more than one YAML document (a second `---` separator)".to_string(),
            "Keep exactly one timeline per file: remove the extra `---` line and the document after it, or merge \
             the documents into one"
                .to_string(),
        ),
        YamlError::Budget { breach, .. } => budget_message(breach),
        YamlError::UnknownAnchor { .. } => (
            "alias refers to an anchor that is not defined (`*name` without a matching `&name` before it)".to_string(),
            "Define the anchor with `&name` before using `*name`, or write the value out".to_string(),
        ),
        YamlError::MergeValueNotMapOrSeqOfMaps { .. } => (
            "the value of a `<<` merge key must be a mapping or a list of mappings".to_string(),
            "Use `<<: *anchor` where the anchor is a mapping".to_string(),
        ),
        YamlError::InvalidUtf8Input => (
            "the document is not valid UTF-8".to_string(),
            "Save the file as UTF-8".to_string(),
        ),
        other => {
            let message = sanitize(&UserMessageFormatter.format_message(other));
            let hint = syntax_hint(&message);
            (message, hint)
        }
    }
}

/// A generic but actionable hint for scanner/parser errors.
fn syntax_hint(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("tab") {
        "YAML indentation must use spaces, not tabs; replace the tab characters with spaces"
            .to_string()
    } else if lower.contains("unclosed") || lower.contains("flow") || lower.contains("bracket") {
        "Check that every `[` and `{` has a matching `]` / `}` and that list items are separated by commas"
            .to_string()
    } else if lower.contains("quote") || lower.contains("quoted") {
        "Check that every quoted string is closed with the same quote character".to_string()
    } else {
        "Check the indentation and the `key: value` syntax around this position; quote values that contain `: ` \
         or ` #` (a comment) or start with a special character such as `*`, `&`, `!`, `%`, `@` or a backtick"
            .to_string()
    }
}

fn budget_message(breach: &BudgetBreach) -> (String, String) {
    match breach {
        BudgetBreach::Depth { depth } => (
            format!("the document is nested too deeply ({depth} levels; the maximum is {MAX_NESTING_DEPTH})"),
            "Timelines need only a few levels of nesting; check the indentation for accidental deep nesting"
                .to_string(),
        ),
        BudgetBreach::Nodes { nodes } => (
            format!("the document has too many values (more than {} nodes)", nodes.saturating_sub(1)),
            "Split the composition into several timelines".to_string(),
        ),
        BudgetBreach::Aliases { .. } | BudgetBreach::Anchors { .. } => (
            "the document uses too many YAML anchors or aliases".to_string(),
            "Write the repeated values out instead of using thousands of `&anchor` / `*alias` references"
                .to_string(),
        ),
        BudgetBreach::Documents { .. } => (
            "the file contains more than one YAML document".to_string(),
            "Keep exactly one timeline per file".to_string(),
        ),
        other => (
            format!("the document exceeds the parser's safety limits ({other:?})"),
            "Reduce the size of the document or the number of anchors/aliases".to_string(),
        ),
    }
}

/// Neutralize control characters in parser-provided text before it reaches a terminal or log.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ---- span map ---------------------------------------------------------------------------------------

/// Alias replays stop adding entries past this size (alias bombs); lookups then fall back to the alias
/// site through [`SpanIndex::locate`].
const MAX_ENTRIES: usize = 250_000;

#[derive(Debug, Clone, Copy)]
struct Pos {
    line: u32,
    column: u32,
}

#[derive(Debug, Clone, Copy)]
struct NodeSpan {
    key: Option<Pos>,
    value: Pos,
    /// Byte range of the key scalar.
    key_bytes: Option<(usize, usize)>,
    /// Byte range of the value when it is a scalar (also through an alias of a scalar anchor).
    value_bytes: Option<(usize, usize)>,
}

fn pos(m: &Marker) -> Pos {
    // granit-parser lines are 1-based and columns 0-based (in characters).
    Pos {
        line: u32::try_from(m.line()).unwrap_or(u32::MAX),
        column: u32::try_from(m.col() + 1).unwrap_or(u32::MAX),
    }
}

/// Byte range of a parser span (str input always has byte offsets).
fn bytes(span: &Span) -> Option<(usize, usize)> {
    Some((span.start.byte_offset()?, span.end.byte_offset()?))
}

/// A plain scalar value whose line continues with ` #`: YAML read the rest of the line as a comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TrailingComment {
    /// Diagnostic path of the value.
    pub path: String,
    /// The scalar as YAML read it.
    pub value: String,
    /// The comment, from `#` to the end of the line (trailing whitespace removed).
    pub comment: String,
    /// 1-based line and character column of the `#`.
    pub line: u32,
    pub column: u32,
}

/// The comment that follows a plain scalar ending at byte `end` on the same line, if any.
fn trailing_comment(text: &str, end: usize, path: &str, value: &str) -> Option<TrailingComment> {
    let rest = text.get(end..)?;
    let gap = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    if gap == 0 || !rest[gap..].starts_with('#') {
        return None;
    }
    let hash = end + gap;
    let line_end = text[hash..]
        .find(['\n', '\r'])
        .map_or(text.len(), |i| hash + i);
    let line_start = text[..hash].rfind('\n').map_or(0, |i| i + 1);
    let line = text[..hash].matches('\n').count() + 1;
    Some(TrailingComment {
        path: path.to_string(),
        value: value.to_string(),
        comment: text[hash..line_end].trim_end().to_string(),
        line: u32::try_from(line).unwrap_or(u32::MAX),
        column: u32::try_from(text[line_start..hash].chars().count() + 1).unwrap_or(u32::MAX),
    })
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// A mapping key whose value comes next.
struct PendingKey {
    text: String,
    pos: Pos,
    /// Byte range of the key scalar.
    bytes: Option<(usize, usize)>,
}

impl PendingKey {
    /// A key that is not tracked (complex keys, non-scalar keys).
    fn untracked(pos: Pos) -> Self {
        PendingKey {
            text: "?".into(),
            pos,
            bytes: None,
        }
    }
}

enum Frame {
    Map {
        path: String,
        key: Option<PendingKey>,
        seen: HashSet<String>,
    },
    Seq {
        path: String,
        next: usize,
    },
    /// `<<: [*a, *b]`
    MergeSeq {
        path: String,
        seen: HashSet<String>,
    },
    /// Complex keys and inline merge sources are not tracked.
    Skip,
}

#[derive(Default)]
struct Anchors {
    /// (anchor id, path, index of the first entry, stack depth)
    open: Vec<(usize, String, usize, usize)>,
    /// anchor id → (path suffix relative to the anchored node, span)
    done: HashMap<usize, Vec<(String, NodeSpan)>>,
}

/// Ordered entries while walking; the index of the first entry of an anchored node identifies its
/// subtree for later alias replays.
#[derive(Default)]
struct Entries {
    list: Vec<(String, NodeSpan)>,
    index: HashMap<String, usize>,
}

impl Entries {
    fn insert(&mut self, path: String, span: NodeSpan) {
        if let Some(&i) = self.index.get(&path) {
            self.list[i].1 = span;
        } else {
            self.index.insert(path.clone(), self.list.len());
            self.list.push((path, span));
        }
    }

    fn insert_if_absent(&mut self, path: String, span: NodeSpan) {
        if !self.index.contains_key(&path) {
            self.insert(path, span);
        }
    }

    fn len(&self) -> usize {
        self.list.len()
    }
}

fn merge(out: &mut Entries, entries: &[(String, NodeSpan)], path: &str, seen: &HashSet<String>) {
    for (suffix, span) in entries {
        let rel = suffix.trim_start_matches('.');
        let top = rel.split(['.', '[']).next().unwrap_or("");
        if !rel.is_empty() && !seen.contains(top) && out.len() < MAX_ENTRIES {
            // Explicit keys and earlier merge sources win.
            out.insert_if_absent(join(path, rel), *span);
        }
    }
}

/// The result of walking a YAML document's events.
struct Walked {
    entries: Entries,
    comments: Vec<TrailingComment>,
}

/// Walk the events of a YAML document that serde-saphyr already accepted. Every path in diagnostic notation
/// gets the value position (and the key position, and byte ranges of scalars). Children reached through
/// aliases or merge keys point at the anchor site. Only the first document is walked.
fn walk(text: &str) -> Walked {
    let mut out = Entries::default();
    let mut comments = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut anchors = Anchors::default();

    for item in Parser::new_from_str(text) {
        // The text was accepted by serde-saphyr; a scanner error here can only mean an unusual construct.
        // Keep what has been collected so far.
        let Ok((event, span)) = item else { break };
        let p = pos(&span.start);
        let (anchor_id, is_container) = match &event {
            Event::Scalar(_, _, anchor, _) | Event::Alias(anchor) => (*anchor, false),
            Event::MappingStart(_, anchor, _) | Event::SequenceStart(_, anchor, _) => {
                (*anchor, true)
            }
            Event::MappingEnd | Event::SequenceEnd => {
                let depth = stack.len();
                stack.pop();
                while anchors.open.last().is_some_and(|a| a.3 == depth) {
                    let Some((id, path, first, _)) = anchors.open.pop() else {
                        break;
                    };
                    let entries = out
                        .list
                        .iter()
                        .skip(first)
                        .filter_map(|(k, v)| {
                            k.strip_prefix(path.as_str()).map(|s| (s.to_owned(), *v))
                        })
                        .collect();
                    anchors.done.insert(id, entries);
                }
                continue;
            }
            Event::DocumentEnd => break,
            _ => continue,
        };
        let is_alias = matches!(event, Event::Alias(_));
        let plain_scalar = match &event {
            Event::Scalar(value, ScalarStyle::Plain, ..) => Some(value.to_string()),
            _ => None,
        };
        let scalar_bytes = match &event {
            Event::Scalar(..) => bytes(&span),
            _ => None,
        };

        let (child_path, key_pos, key_bytes) = match stack.last_mut() {
            None => (String::new(), None, None),
            Some(Frame::Skip) => {
                if is_container {
                    stack.push(Frame::Skip);
                }
                continue;
            }
            Some(Frame::MergeSeq { path, seen }) => {
                if is_alias {
                    if let Some(entries) = anchors.done.get(&anchor_id) {
                        merge(&mut out, entries, path, seen);
                    }
                } else if is_container {
                    stack.push(Frame::Skip);
                }
                continue;
            }
            Some(Frame::Seq { path, next }) => {
                *next += 1;
                (format!("{path}[{}]", *next - 1), None, None)
            }
            Some(Frame::Map { path, key, seen }) => match key.take() {
                None => {
                    // This node is a key.
                    match &event {
                        Event::Scalar(text, ..) => {
                            *key = Some(PendingKey {
                                text: text.to_string(),
                                pos: p,
                                bytes: scalar_bytes,
                            })
                        }
                        _ if is_container => {
                            *key = Some(PendingKey::untracked(p));
                            stack.push(Frame::Skip);
                        }
                        _ => *key = Some(PendingKey::untracked(p)),
                    }
                    continue;
                }
                Some(k) if k.text == "<<" => {
                    if is_alias {
                        if let Some(entries) = anchors.done.get(&anchor_id) {
                            merge(&mut out, entries, path, seen);
                        }
                    } else if matches!(event, Event::SequenceStart(..)) {
                        let (path, seen) = (path.clone(), seen.clone());
                        stack.push(Frame::MergeSeq { path, seen });
                    } else if is_container {
                        stack.push(Frame::Skip);
                    }
                    continue;
                }
                Some(k) => {
                    seen.insert(k.text.clone());
                    (join(path, &k.text), Some(k.pos), k.bytes)
                }
            },
        };

        let mut node = NodeSpan {
            key: key_pos,
            value: p,
            key_bytes,
            value_bytes: scalar_bytes,
        };
        if let (Some(value), Some((_, end))) = (&plain_scalar, scalar_bytes)
            && let Some(comment) = trailing_comment(text, end, &child_path, value)
        {
            comments.push(comment);
        }
        if is_alias {
            // An alias of a scalar anchor carries the anchored scalar's bytes.
            if let Some(entries) = anchors.done.get(&anchor_id) {
                node.value_bytes = entries
                    .iter()
                    .find(|(suffix, _)| suffix.is_empty())
                    .and_then(|(_, span)| span.value_bytes);
            }
        }
        if out.len() < MAX_ENTRIES {
            out.insert(child_path.clone(), node);
        }
        if is_alias {
            if let Some(entries) = anchors.done.get(&anchor_id) {
                for (suffix, span) in entries.iter().filter(|(s, _)| !s.is_empty()) {
                    if out.len() >= MAX_ENTRIES {
                        break;
                    }
                    out.insert(format!("{child_path}{suffix}"), *span);
                }
            }
            continue;
        }
        if anchor_id > 0 {
            anchors.open.push((
                anchor_id,
                child_path.clone(),
                out.len().saturating_sub(1),
                stack.len() + 1,
            ));
            if !is_container {
                // A scalar anchor closes immediately; aliases of it reuse the scalar's bytes.
                if let Some((id, ..)) = anchors.open.pop() {
                    anchors.done.insert(id, vec![(String::new(), node)]);
                }
            }
        }
        match event {
            Event::MappingStart(..) => stack.push(Frame::Map {
                path: child_path,
                key: None,
                seen: HashSet::new(),
            }),
            Event::SequenceStart(..) => stack.push(Frame::Seq {
                path: child_path,
                next: 0,
            }),
            _ => {}
        }
    }
    Walked {
        entries: out,
        comments,
    }
}

/// The span map of a YAML document that serde-saphyr already accepted (see [`SpanIndex`]), and the plain
/// scalars that a ` #` comment cut short.
pub(super) fn index(text: &str) -> (SpanIndex, Vec<TrailingComment>) {
    let walked = walk(text);
    let mut index = SpanIndex::new();
    for (path, span) in walked.entries.list {
        index.insert(path.clone(), span.value.line, span.value.column);
        if let Some(key) = span.key {
            index.insert_key(path, key.line, key.column);
        }
    }
    (index, walked.comments)
}

/// The span map alone (see [`index`]).
#[cfg(test)]
pub(super) fn spans(text: &str) -> SpanIndex {
    index(text).0
}

/// Byte ranges of the scalar values at `values` (a mapping or list value contributes every key and scalar
/// inside it) and of the keys of the entries at `keys`. Aliased values point at the anchor site.
pub(super) fn ranges(
    text: &str,
    values: &HashSet<String>,
    keys: &HashSet<String>,
) -> Vec<Range<usize>> {
    let walked = walk(text);
    let mut out = Vec::new();
    let inside = |path: &str| {
        values.iter().any(|v| {
            path.strip_prefix(v.as_str())
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
        })
    };
    for (path, span) in &walked.entries.list {
        if values.contains(path) || inside(path) {
            out.extend(span.value_bytes.map(|(a, b)| a..b));
            if inside(path) {
                out.extend(span.key_bytes.map(|(a, b)| a..b));
            }
        }
        if keys.contains(path) {
            out.extend(span.key_bytes.map(|(a, b)| a..b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_for_nested_paths_and_keys() {
        let src = "version: \"1.0\"\ntracks:\n  - id: a\n    type: speech\n    sequence:\n      - text: hi\n        x-note: 1\n";
        let s = spans(src);
        assert_eq!(s.get("version"), Some((1, 10)));
        assert_eq!(s.key("version"), Some((1, 1)));
        assert_eq!(s.get("tracks[0].id"), Some((3, 9)));
        assert_eq!(s.key("tracks[0].id"), Some((3, 5)));
        assert_eq!(s.get("tracks[0].sequence[0].text"), Some((6, 15)));
        assert_eq!(s.key("tracks[0].sequence[0].x-note"), Some((7, 9)));
        assert_eq!(s.locate("tracks[0].sequence[0].missing"), Some((6, 9)));
    }

    #[test]
    fn spans_follow_aliases_and_merges() {
        let src = "defaults: &d\n  provider: mock\nspeakers:\n  a: *d\n  b:\n    <<: *d\n    voice_id: x\n";
        let s = spans(src);
        assert_eq!(s.get("defaults.provider"), Some((2, 13)));
        // Alias children point at the anchor site.
        assert_eq!(s.get("speakers.a.provider"), Some((2, 13)));
        assert_eq!(s.get("speakers.b.provider"), Some((2, 13)));
        assert_eq!(s.get("speakers.b.voice_id"), Some((7, 15)));
    }

    #[test]
    fn errors_have_positions() {
        let d = parse("a: 1\na: 2\n").unwrap_err();
        assert_eq!(d.code, "yaml_syntax");
        assert!(d.message.contains("duplicate key `a`"), "{}", d.message);
        assert_eq!(d.line, Some(2));

        let d = parse("a: 1\n---\nb: 2\n").unwrap_err();
        assert!(
            d.message.contains("more than one YAML document"),
            "{}",
            d.message
        );
        assert!(d.line.is_some_and(|l| l >= 2), "{d:?}");

        let d = parse("a:\n\t- b\n").unwrap_err();
        assert_eq!(d.code, "yaml_syntax");
        assert_eq!(d.line, Some(2));
        assert!(d.hint.as_deref().unwrap_or("").contains("tab"), "{d:?}");
    }

    #[test]
    fn strict_booleans_and_non_finite() {
        let v = parse("a: yes\nb: true\nc: .inf\n").unwrap();
        assert_eq!(v["a"], "yes");
        assert_eq!(v["b"], true);
        assert_eq!(v["c"], ".inf");
    }

    #[test]
    fn depth_limit() {
        let mut src = String::new();
        for i in 0..70 {
            src.push_str(&" ".repeat(i * 2));
            src.push_str("a:\n");
        }
        src.push_str(&" ".repeat(140));
        src.push_str("b: 1\n");
        let d = parse(&src).unwrap_err();
        assert!(d.message.contains("nested too deeply"), "{}", d.message);
    }
}
