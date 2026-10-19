//! rustc-style rendering of [`Diagnostic`]s against the timeline text:
//!
//! ```text
//! error[unknown_reference]: `trigger_tracks[0]` refers to `voise`, which is not a track id
//!   --> timeline.yaml:18:24
//!    |
//! 18 |       trigger_tracks: [voise, cue_01, bed]
//!    |                        ^^^^^
//!    |
//!    = hint: Did you mean `voice`? Track ids: `bed`, `voice`
//! ```
//!
//! The caret underlines the YAML/JSON token at the diagnostic's column (a quoted string, a flow
//! collection, a key or a plain scalar). Diagnostics without a position show the document path instead,
//! and those with neither (about the environment or the command line, e.g. an `-o` file that must not be
//! overwritten) show no location at all.
//!
//! Secrets never reach the terminal: every value, key or token that core's
//! [`secret_spans`](soniccraft_core::parse::secret_spans) reports is shown as `<redacted>` in snippets
//! (a quoted value keeps its quotes), and the caret is moved accordingly. The token under the caret of a
//! `secret_in_timeline` diagnostic is redacted as well.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use soniccraft_core::diagnostics::codes;
use soniccraft_core::{Diagnostic, Severity, SourceFormat};

use crate::format::{char_width, plural};
use crate::term::{Style, paint};

/// Lines wider than this many columns are shown as a window around the caret.
const MAX_LINE_COLS: usize = 110;
/// Columns kept before the caret when a long line is cut.
const LEAD_COLS: usize = 40;
/// What a snippet shows instead of a secret (the marker the JSON reports use too).
const REDACTED: &str = "<redacted>";
/// Lines longer than this many bytes are prepared once and cached: a minified JSON timeline is a single
/// line, and each of its diagnostics must not rescan it.
const LONG_LINE_BYTES: usize = 1024;
/// Chars of a long line considered before the caret (a window shows `LEAD_COLS` columns of them)...
const REGION_BEFORE: usize = 4 * LEAD_COLS;
/// ... and from the caret on (a window shows at most `MAX_LINE_COLS` columns).
const REGION_AFTER: usize = 4 * MAX_LINE_COLS;

/// The timeline text, split into lines, plus the name shown after `-->`.
#[derive(Debug, Clone)]
pub(crate) struct SourceView<'a> {
    name: &'a str,
    /// The text as given, including a byte-order mark: what the secret scan measures.
    text: &'a str,
    format: SourceFormat,
    /// Every line (without its terminator) and the byte offset in `text` where it starts.
    lines: Vec<(usize, &'a str)>,
    /// Whether the text ends with a newline (then "line N+1, column 1" is the end of the file).
    trailing_newline: bool,
    /// JSON text: every value is in a flow context (`,` `]` `}` end plain scalars).
    json: bool,
    /// Byte ranges of the secrets in `text` (sorted, disjoint), computed when a snippet first needs
    /// them: the scan parses the document a second time.
    secrets: OnceCell<Vec<Range<usize>>>,
    /// Long lines already prepared, by line number.
    prepared: RefCell<HashMap<u32, Rc<Prepared>>>,
}

impl<'a> SourceView<'a> {
    /// A view of `text`, displayed as `name` (`timeline.yaml`, `<stdin>`).
    pub(crate) fn new(name: &'a str, text: &'a str, format: SourceFormat) -> Self {
        let bom = if text.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        };
        let body = &text[bom..];
        // The same lines as `str::lines` (`\n` and `\r\n` end a line), with their offsets.
        let mut lines = Vec::new();
        let mut offset = bom;
        for piece in body.split_inclusive('\n') {
            let line = match piece.strip_suffix('\n') {
                Some(line) => line.strip_suffix('\r').unwrap_or(line),
                None => piece,
            };
            lines.push((offset, line));
            offset += piece.len();
        }
        SourceView {
            name,
            text,
            format,
            lines,
            trailing_newline: body.is_empty() || body.ends_with('\n'),
            json: format.resolve(text) == SourceFormat::Json,
            secrets: OnceCell::new(),
            prepared: RefCell::new(HashMap::new()),
        }
    }

    /// Line `n` (1-based) and its byte offset; the empty line after a final newline counts as the last
    /// line.
    fn line(&self, n: u32) -> Option<(usize, &'a str)> {
        let idx = usize::try_from(n).ok()?.checked_sub(1)?;
        match self.lines.get(idx) {
            Some(&line) => Some(line),
            None if idx == self.lines.len() && self.trailing_newline => Some((self.text.len(), "")),
            None => None,
        }
    }

    /// Line `n` ready for a snippet (cached when it is long).
    fn prepare(&self, n: u32) -> Option<Rc<Prepared>> {
        let (offset, line) = self.line(n)?;
        let long = line.len() > LONG_LINE_BYTES;
        if long && let Some(prepared) = self.prepared.borrow().get(&n) {
            return Some(Rc::clone(prepared));
        }
        let prepared = Rc::new(Prepared::new(
            line,
            &self.secrets_in(offset, line),
            self.json,
        ));
        if long {
            self.prepared.borrow_mut().insert(n, Rc::clone(&prepared));
        }
        Some(prepared)
    }

    /// The secrets on the line that starts at byte `offset`, as byte ranges of `line` (sorted, disjoint,
    /// on char boundaries) and whether each starts and ends on this line.
    fn secrets_in(&self, offset: usize, line: &str) -> Vec<(Range<usize>, bool)> {
        let secrets = self
            .secrets
            .get_or_init(|| soniccraft_core::parse::secret_spans(self.text, self.format));
        let end = offset + line.len();
        let first = secrets.partition_point(|r| r.end <= offset);
        let mut out: Vec<(Range<usize>, bool)> = Vec::new();
        for r in secrets[first..].iter().take_while(|r| r.start < end) {
            let mut a = r.start.max(offset) - offset;
            let mut b = r.end.min(end) - offset;
            // Ranges are on char boundaries; widen defensively rather than cut a char.
            while !line.is_char_boundary(a) {
                a -= 1;
            }
            while !line.is_char_boundary(b) {
                b += 1;
            }
            let whole = r.start >= offset && r.end <= end;
            match out.last_mut() {
                Some((last, last_whole)) if a <= last.end => {
                    last.end = last.end.max(b);
                    *last_whole = *last_whole && whole;
                }
                _ if a < b => out.push((a..b, whole)),
                _ => {}
            }
        }
        out
    }
}

/// A source line prepared for snippets.
#[derive(Debug)]
struct Prepared {
    chars: Vec<char>,
    /// The line's secrets as char ranges (sorted, disjoint).
    secrets: Vec<Secret>,
    /// YAML: `flow[i]` tells whether char position `i` (`0..=len`) is inside a flow collection opened
    /// earlier on the line. None for JSON, where every value is.
    flow: Option<Vec<bool>>,
}

impl Prepared {
    fn new(line: &str, secrets: &[(Range<usize>, bool)], json: bool) -> Self {
        // Byte offsets to char indices in one pass (the ranges are sorted and disjoint).
        let mut converted = Vec::with_capacity(secrets.len());
        let (mut byte, mut counted) = (0, 0);
        let mut to_chars = |target: usize| {
            counted += line[byte..target].chars().count();
            byte = target;
            counted
        };
        for (range, whole) in secrets {
            let start = to_chars(range.start);
            let end = to_chars(range.end);
            converted.push(Secret {
                chars: start..end,
                whole: *whole,
            });
        }
        let chars: Vec<char> = line.chars().collect();
        let flow = (!json).then(|| flow_flags(&chars));
        Prepared {
            chars,
            secrets: converted,
            flow,
        }
    }

    /// Whether position `pos` is in a flow context.
    fn in_flow(&self, pos: usize) -> bool {
        match &self.flow {
            None => true,
            Some(flags) => flags[pos.min(flags.len() - 1)],
        }
    }

    /// The secrets that overlap chars `range`.
    fn secrets_overlapping(&self, range: &Range<usize>) -> &[Secret] {
        let first = self.secrets.partition_point(|s| s.chars.end <= range.start);
        let last = self.secrets.partition_point(|s| s.chars.start < range.end);
        &self.secrets[first..last.max(first)]
    }
}

/// A secret in one source line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Secret {
    /// The chars it covers.
    chars: Range<usize>,
    /// It starts and ends on this line (a quoted value then keeps its quotes).
    whole: bool,
}

/// A source line with its secrets replaced by [`REDACTED`].
#[derive(Debug)]
struct Masked {
    chars: Vec<char>,
    /// (original chars, replacement chars) of every replaced range, in order.
    replaced: Vec<(Range<usize>, Range<usize>)>,
}

impl Masked {
    /// `line` with `secrets` (char ranges, in any order, possibly overlapping) replaced. Whitespace at
    /// either end of a range stays (block-scalar indentation), and a quoted value keeps its quotes. The
    /// chars of a secret are never copied, so a huge one costs nothing.
    fn new(line: &[char], secrets: &[Secret]) -> Self {
        let mut secrets: Vec<Secret> = secrets
            .iter()
            .filter_map(|s| {
                let (mut a, mut b) = (s.chars.start.min(line.len()), s.chars.end.min(line.len()));
                while a < b && line[a].is_whitespace() {
                    a += 1;
                }
                while b > a && line[b - 1].is_whitespace() {
                    b -= 1;
                }
                (a < b).then_some(Secret {
                    chars: a..b,
                    whole: s.whole,
                })
            })
            .collect();
        secrets.sort_by_key(|s| (s.chars.start, s.chars.end));
        let mut merged: Vec<Secret> = Vec::with_capacity(secrets.len());
        for s in secrets {
            match merged.last_mut() {
                Some(last) if s.chars.start <= last.chars.end => {
                    // Overlapping or adjacent secrets are masked as one range.
                    last.whole = last.whole && s.whole;
                    last.chars.end = last.chars.end.max(s.chars.end);
                }
                _ => merged.push(s),
            }
        }
        let mut chars = Vec::with_capacity(line.len().min(4 * MAX_LINE_COLS));
        let mut replaced = Vec::with_capacity(merged.len());
        let mut next = 0;
        for s in merged {
            chars.extend_from_slice(&line[next..s.chars.start]);
            let first = line[s.chars.start];
            let quoted = s.whole
                && matches!(first, '"' | '\'')
                && s.chars.len() >= 2
                && line[s.chars.end - 1] == first;
            let start = chars.len();
            if quoted {
                chars.push(first);
            }
            chars.extend(REDACTED.chars());
            if quoted {
                chars.push(first);
            }
            replaced.push((s.chars.clone(), start..chars.len()));
            next = s.chars.end;
        }
        chars.extend_from_slice(&line[next..]);
        Masked { chars, replaced }
    }

    /// Where position `pos` of the original line (a boundary between chars, `0..=len` or beyond) is in
    /// the masked line. A position inside a replaced range maps to the start of its replacement, or to
    /// the end with `end`.
    fn map(&self, pos: usize, end: bool) -> usize {
        let mut shifted = pos;
        for (original, replacement) in &self.replaced {
            if pos <= original.start {
                break;
            }
            if pos < original.end {
                return if end {
                    replacement.end
                } else {
                    replacement.start
                };
            }
            shifted = pos - original.end + replacement.end;
        }
        shifted
    }
}

/// Which severities to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Filter {
    /// Print warnings (`-q` hides them).
    pub(crate) warnings: bool,
    /// Print info diagnostics (`-v` shows them).
    pub(crate) infos: bool,
    /// Add an `= at: <path>` note to positioned diagnostics (`-v`).
    pub(crate) paths: bool,
}

/// The rendered diagnostics plus what was (not) shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Rendered {
    /// The blocks, each followed by a blank line.
    pub(crate) text: String,
    pub(crate) errors: usize,
    pub(crate) warnings: usize,
    /// Info diagnostics that were printed.
    pub(crate) infos: usize,
    /// Diagnostics filtered out (infos without `-v`, warnings with `-q`).
    pub(crate) hidden_infos: usize,
    pub(crate) hidden_warnings: usize,
}

impl Rendered {
    /// `2 errors, 1 warning` (with `, 3 notes` when infos were shown), or `no problems`.
    pub(crate) fn counts(&self) -> String {
        counts_phrase(self.errors, self.warnings, self.infos)
    }

    /// ` (3 notes hidden, use -v to show them)` when infos were filtered out, else empty.
    pub(crate) fn hidden_note(&self) -> String {
        if self.hidden_infos == 0 {
            String::new()
        } else {
            format!(
                " ({} hidden, use -v to show)",
                plural(self.hidden_infos, "note", "notes")
            )
        }
    }
}

/// `2 errors, 1 warning, 3 notes`, or `no problems`.
pub(crate) fn counts_phrase(errors: usize, warnings: usize, notes: usize) -> String {
    let mut parts = Vec::new();
    if errors > 0 {
        parts.push(plural(errors, "error", "errors"));
    }
    if warnings > 0 {
        parts.push(plural(warnings, "warning", "warnings"));
    }
    if notes > 0 {
        parts.push(plural(notes, "note", "notes"));
    }
    if parts.is_empty() {
        "no problems".to_string()
    } else {
        parts.join(", ")
    }
}

/// Render every diagnostic that passes `filter`, in the given order, each followed by a blank line.
pub(crate) fn render_all(
    diagnostics: &[Diagnostic],
    view: Option<&SourceView<'_>>,
    color: bool,
    filter: Filter,
) -> Rendered {
    render_all_with(diagnostics, view, color, filter, &|_| true)
}

/// [`render_all`], rendering the diagnostics for which `in_document` is false without a location (no
/// `-->` line, no snippet, whatever path they carry): they are about something else than the document,
/// e.g. a render option.
pub(crate) fn render_all_with(
    diagnostics: &[Diagnostic],
    view: Option<&SourceView<'_>>,
    color: bool,
    filter: Filter,
    in_document: &dyn Fn(&Diagnostic) -> bool,
) -> Rendered {
    let mut out = Rendered::default();
    for d in diagnostics {
        let shown = match d.severity {
            Severity::Error => {
                out.errors += 1;
                true
            }
            Severity::Warning if filter.warnings => {
                out.warnings += 1;
                true
            }
            Severity::Warning => {
                out.hidden_warnings += 1;
                false
            }
            Severity::Info if filter.infos => {
                out.infos += 1;
                true
            }
            Severity::Info => {
                out.hidden_infos += 1;
                false
            }
        };
        if shown {
            let block = if in_document(d) {
                render(d, view, color, filter.paths)
            } else {
                let detached = Diagnostic {
                    path: None,
                    line: None,
                    column: None,
                    ..d.clone()
                };
                render(&detached, None, color, filter.paths)
            };
            out.text.push_str(&block);
            out.text.push('\n');
        }
    }
    out
}

/// Render one diagnostic block (without the trailing blank line).
pub(crate) fn render(
    d: &Diagnostic,
    view: Option<&SourceView<'_>>,
    color: bool,
    show_path: bool,
) -> String {
    let severity_style = match d.severity {
        Severity::Error => Style::Error,
        Severity::Warning => Style::Warning,
        Severity::Info => Style::Info,
    };
    let line_no = d.line.filter(|&l| l > 0);
    let gutter_width = line_no.map_or(1, |l| l.to_string().len());
    let pad = " ".repeat(gutter_width);
    let gutter = |s: &str| paint(color, Style::Gutter, s);

    let mut out = String::new();
    let mut message = d.message.lines();
    out.push_str(&paint(
        color,
        severity_style,
        &format!("{}[{}]", d.severity, d.code),
    ));
    out.push_str(&paint(
        color,
        Style::Bold,
        &format!(": {}", message.next().unwrap_or("")),
    ));
    out.push('\n');
    for more in message {
        out.push_str(&format!("{pad}  {more}\n"));
    }

    // Location line.
    let location = match (view, line_no) {
        (Some(v), Some(line)) => Some(match d.column.filter(|&c| c > 0) {
            Some(col) => format!("{}:{line}:{col}", v.name),
            None => format!("{}:{line}", v.name),
        }),
        (Some(v), None) => d
            .path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|path| format!("{} ({path})", v.name)),
        (None, _) => d.path.clone().filter(|p| !p.is_empty()),
    };
    if let Some(location) = &location {
        out.push_str(&format!("{pad}{} {location}\n", gutter("-->")));
    }

    // Snippet with caret.
    let source_line = match (view, line_no) {
        (Some(v), Some(line)) => v.prepare(line).map(|prepared| (line, prepared)),
        _ => None,
    };
    let mut snippet_shown = false;
    if let Some((line, prepared)) = source_line {
        let col = d.column.filter(|&c| c > 0).map(|c| c as usize - 1);
        let shown = excerpt(&prepared, col, d.code == codes::SECRET_IN_TIMELINE);
        out.push_str(&format!("{pad} {}\n", gutter("|")));
        let body = if shown.text.is_empty() {
            String::new()
        } else {
            format!(" {}", shown.text)
        };
        out.push_str(&format!(
            "{} {}{body}\n",
            gutter(&line.to_string()),
            gutter("|")
        ));
        if let Some((offset, len)) = shown.caret {
            out.push_str(&format!(
                "{pad} {} {}{}\n",
                gutter("|"),
                " ".repeat(offset),
                paint(color, severity_style, &"^".repeat(len.max(1)))
            ));
        }
        snippet_shown = true;
    }

    // Notes.
    let mut notes: Vec<(&str, &str)> = Vec::new();
    if show_path
        && line_no.is_some()
        && let Some(path) = d.path.as_deref().filter(|p| !p.is_empty())
    {
        notes.push(("at", path));
    }
    if let Some(hint) = d.hint.as_deref().filter(|h| !h.trim().is_empty()) {
        notes.push(("hint", hint));
    }
    if !notes.is_empty() {
        if snippet_shown {
            out.push_str(&format!("{pad} {}\n", gutter("|")));
        }
        for (label, text) in notes {
            let mut lines = text.lines();
            out.push_str(&format!(
                "{pad} {} {}: {}\n",
                gutter("="),
                paint(color, Style::Bold, label),
                lines.next().unwrap_or("")
            ));
            let indent = " ".repeat(label.len() + 2);
            for more in lines {
                out.push_str(&format!("{pad}   {indent}{more}\n"));
            }
        }
    }
    out
}

/// A display-ready source line: secrets redacted, tabs expanded, long lines cut around the caret.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Excerpt {
    text: String,
    /// Columns before the caret and caret length, when the diagnostic has a column.
    caret: Option<(usize, usize)>,
}

/// `line` for display with the caret at char `col`. Its secrets are replaced by [`REDACTED`]; with
/// `redact_caret`, so is the token under the caret. Only the chars near the caret of a long line are
/// looked at, so the cost does not grow with the line.
fn excerpt(line: &Prepared, col: Option<usize>, redact_caret: bool) -> Excerpt {
    let original = &line.chars;
    let len = original.len();
    let short = len <= REGION_BEFORE + REGION_AFTER;
    // The caret token is measured on the source text (up to the end of the region), then mapped into the
    // masked line.
    let token = col.map(|c| {
        let start = c.min(len);
        let limit = if short {
            len
        } else {
            (start + REGION_AFTER).min(len)
        };
        let end = start + token_len(&original[..limit], start, line.in_flow(start));
        (start, end)
    });
    let anchor = token.map_or(0, |(start, _)| start);
    // The chars that may be shown: the whole line, or a region around the caret, widened to whole
    // secrets (each shrinks to one marker, so the widening adds nothing to show).
    let mut region = if short {
        0..len
    } else {
        anchor.saturating_sub(REGION_BEFORE)..(anchor + REGION_AFTER).min(len)
    };
    let mut secrets = line.secrets_overlapping(&region).to_vec();
    if redact_caret && let Some((start, end)) = token {
        secrets.push(Secret {
            chars: start..end.min(len),
            whole: true,
        });
    }
    for s in &secrets {
        region.start = region.start.min(s.chars.start);
        region.end = region.end.max(s.chars.end.min(len));
    }
    let base = region.start;
    let shifted: Vec<Secret> = secrets
        .iter()
        .map(|s| Secret {
            chars: s.chars.start - base..s.chars.end.min(len) - base,
            whole: s.whole,
        })
        .collect();
    let masked = Masked::new(&original[region.clone()], &shifted);
    let chars = &masked.chars;
    let start = token.map(|(s, _)| masked.map(s - base, false));
    let caret_len = token.map(|(s, e)| {
        masked
            .map(e - base, true)
            .saturating_sub(masked.map(s - base, false))
    });

    // Display widths per char (tabs as 4 columns).
    let widths: Vec<usize> = chars
        .iter()
        .map(|&c| if c == '\t' { 4 } else { char_width(c) })
        .collect();
    let total: usize = widths.iter().sum();

    // Window [from, to) in chars.
    let (mut from, mut to) = (0, chars.len());
    if total > MAX_LINE_COLS {
        let anchor = start.unwrap_or(0);
        // Walk back LEAD_COLS columns from the anchor.
        let mut cols = 0;
        from = anchor;
        while from > 0 && cols + widths[from - 1] <= LEAD_COLS {
            from -= 1;
            cols += widths[from];
        }
        // Walk forward until the window is full.
        let mut used: usize = widths[from..anchor.min(chars.len())].iter().sum();
        to = anchor.min(chars.len());
        while to < chars.len() && used + widths[to] <= MAX_LINE_COLS {
            used += widths[to];
            to += 1;
        }
    }
    let mut text = String::new();
    let mut prefix_cols = 0;
    if from > 0 || region.start > 0 {
        text.push_str("...");
        prefix_cols = 3;
    }
    for &c in &chars[from..to] {
        if c == '\t' {
            text.push_str("    ");
        } else {
            text.push(c);
        }
    }
    if to < chars.len() || region.end < len {
        text.push_str("...");
    }
    let text = text.trim_end().to_string();

    let caret = start.map(|s| {
        let s = s.max(from);
        let offset = prefix_cols + widths[from..s.min(to)].iter().sum::<usize>();
        let end = (s + caret_len.unwrap_or(1)).min(to);
        let cols: usize = widths[s.min(to)..end].iter().sum();
        (offset, cols.max(1))
    });
    Excerpt { text, caret }
}

/// Length in chars of the YAML/JSON token starting at `start`: a quoted string (with its quotes), a flow
/// collection on one line, or a plain scalar / key (up to `: `, ` #`, end of line, and in a flow context
/// (`flow`) `,` `]` `}`). At least 1. `chars` may be cut short: then so is the token.
fn token_len(chars: &[char], start: usize, flow: bool) -> usize {
    let Some(&first) = chars.get(start) else {
        return 1;
    };
    let rest = chars.len() - start;
    match first {
        '"' => {
            let mut i = start + 1;
            while i < chars.len() {
                match chars[i] {
                    '\\' => i += 2,
                    '"' => return i + 1 - start,
                    _ => i += 1,
                }
            }
            rest
        }
        '\'' => {
            let mut i = start + 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    return i + 1 - start;
                }
                i += 1;
            }
            rest
        }
        '{' | '[' => matching_bracket(chars, start).map_or_else(
            || trimmed_len(chars, start, chars.len()),
            |end| end + 1 - start,
        ),
        _ => {
            let mut end = start;
            while end < chars.len() {
                let c = chars[end];
                if c == '#' && end > start && chars[end - 1].is_whitespace() {
                    break;
                }
                if c == ':' && chars.get(end + 1).is_none_or(|n| n.is_whitespace()) {
                    break;
                }
                if flow && matches!(c, ',' | ']' | '}') {
                    break;
                }
                end += 1;
            }
            trimmed_len(chars, start, end)
        }
    }
}

/// `end - start` without trailing whitespace, at least 1.
fn trimmed_len(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    (end - start).max(1)
}

/// Index of the bracket closing the one at `open` on the same line (quotes respected).
fn matching_bracket(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some('"') if c == '\\' => i += 1,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '{' | '[' => depth += 1,
                '}' | ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// For every position `0..=len` of a YAML line: whether an unclosed `{` or `[` precedes it (quotes
/// respected; nothing after a ` #` comment opens or closes anything).
fn flow_flags(chars: &[char]) -> Vec<bool> {
    let mut flags = Vec::with_capacity(chars.len() + 1);
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    // Once a comment starts, every later position gets the answer at its start.
    let mut comment: Option<bool> = None;
    for (i, &c) in chars.iter().enumerate() {
        flags.push(comment.unwrap_or(depth > 0));
        if comment.is_some() {
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '#' if i == 0 || chars[i - 1].is_whitespace() => comment = Some(depth > 0),
                '{' | '[' => depth += 1,
                '}' | ']' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
    }
    flags.push(comment.unwrap_or(depth > 0));
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = "version: \"1.0\"\noutput:\n  format: flack\n  export_subtitles: [vvt, srt]\ntracks:\n  - id: bed\n    source: \"bed one.wav\" # the bed\n    tex: hi\n";

    fn diag(code: &str, line: u32, column: u32) -> Diagnostic {
        Diagnostic::error(code, "something is wrong").at(line, column)
    }

    fn view(text: &str) -> SourceView<'_> {
        SourceView::new("timeline.yaml", text, SourceFormat::Yaml)
    }

    #[test]
    fn rustc_layout() {
        let d = Diagnostic::error("invalid_value", "unknown output format `flack`")
            .with_path("output.format")
            .with_hint("Did you mean `flac`?")
            .at(3, 11);
        let got = render(&d, Some(&view(YAML)), false, false);
        assert_eq!(
            got,
            "error[invalid_value]: unknown output format `flack`\n \
             --> timeline.yaml:3:11\n  \
             |\n\
             3 |   format: flack\n  \
             |           ^^^^^\n  \
             |\n  \
             = hint: Did you mean `flac`?\n"
        );
    }

    /// (column of the first `^`, number of `^`) in a rendered caret line.
    fn caret_span(line: &str) -> (usize, usize) {
        let start = line.find('^').unwrap_or(0);
        let len = line[start..].chars().take_while(|&c| c == '^').count();
        (start, len)
    }

    #[test]
    fn two_digit_gutter_and_path_note() {
        let text = format!("{}  bad: [a, b]\n", "x: 1\n".repeat(11));
        let d = Diagnostic::warning("unknown_reference", "`b` is unknown")
            .with_path("bad[1]")
            .at(12, 12);
        let got = render(&d, Some(&view(&text)), false, true);
        let expected = format!(
            "warning[unknown_reference]: `b` is unknown\n  --> timeline.yaml:12:12\n   |\n12 |   bad: [a, b]\n   |{}^\n   |\n   = at: bad[1]\n",
            " ".repeat(12)
        );
        assert_eq!(got, expected);
        // The caret sits under `b` in the source line.
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(caret_span(lines[4]).0, lines[3].rfind('b').unwrap_or(0));
    }

    #[test]
    fn caret_covers_tokens() {
        let v = view(YAML);
        let caret = |line: u32, col: u32| {
            let rendered = render(&diag("x", line, col), Some(&v), false, false);
            let caret_line = rendered.lines().nth(4).unwrap_or_default().to_string();
            caret_line
                .trim_start_matches(' ')
                .trim_start_matches('|')
                .to_string()
        };
        // flow sequence item
        assert_eq!(caret(4, 22).trim(), "^^^");
        // quoted scalar with a trailing comment
        assert_eq!(caret(7, 13).trim(), "^^^^^^^^^^^^^");
        // key position (unknown_field)
        assert_eq!(caret(8, 5).trim(), "^^^");
        // whole flow collection
        assert_eq!(caret(4, 21).trim(), "^^^^^^^^^^");
    }

    #[test]
    fn json_values_stop_at_commas() {
        let text = "{\n  \"output\": {\"channels\": 3, \"sample_rate\": \"48k\"}\n}\n";
        let v = SourceView::new("t.json", text, SourceFormat::Json);
        let check = |rendered: &str, token: &str| {
            let lines: Vec<&str> = rendered.lines().collect();
            let at = lines[3].find(token).unwrap_or(usize::MAX);
            assert_eq!(
                caret_span(lines[4]),
                (at, token.chars().count()),
                "{rendered}"
            );
        };
        let col_of =
            |token: &str| text.lines().nth(1).and_then(|l| l.find(token)).unwrap_or(0) as u32 + 1;
        check(
            &render(&diag("x", 2, col_of("3,")), Some(&v), false, false),
            "3",
        );
        check(
            &render(&diag("x", 2, col_of("\"48k\"")), Some(&v), false, false),
            "\"48k\"",
        );
        // pretty JSON: a number at the end of a line keeps its comma out
        let pretty = "{\n  \"channels\": 3,\n}\n";
        let v = SourceView::new("t.json", pretty, SourceFormat::Json);
        check(&render(&diag("x", 2, 15), Some(&v), false, false), "3");
    }

    #[test]
    fn end_of_file_position() {
        let text = "version: \"1.0\"\ntracks:\n";
        let rendered = render(&diag("yaml_syntax", 3, 1), Some(&view(text)), false, false);
        assert_eq!(
            rendered,
            "error[yaml_syntax]: something is wrong\n --> timeline.yaml:3:1\n  |\n3 |\n  | ^\n"
        );
        // A position past the end shows no snippet.
        let rendered = render(&diag("yaml_syntax", 9, 1), Some(&view(text)), false, false);
        assert_eq!(
            rendered,
            "error[yaml_syntax]: something is wrong\n --> timeline.yaml:9:1\n"
        );
    }

    #[test]
    fn unpositioned_diagnostics_show_the_path() {
        let d = Diagnostic::error("missing_credentials", "ELEVENLABS_API_KEY is not set")
            .with_path("providers.elevenlabs")
            .with_hint("export ELEVENLABS_API_KEY=...");
        let rendered = render(&d, Some(&view(YAML)), false, false);
        assert_eq!(
            rendered,
            "error[missing_credentials]: ELEVENLABS_API_KEY is not set\n \
             --> timeline.yaml (providers.elevenlabs)\n  \
             = hint: export ELEVENLABS_API_KEY=...\n"
        );
        let bare = Diagnostic::warning("silent_output", "the master is silent");
        assert_eq!(
            render(&bare, None, false, false),
            "warning[silent_output]: the master is silent\n"
        );
        // Nothing to point at in the document: no `--> timeline.yaml` either.
        let option = Diagnostic::error(
            "invalid_value",
            "refusing to overwrite `notes.txt`: it is not a SonicCraft output",
        );
        assert_eq!(
            render(&option, Some(&view(YAML)), false, false),
            "error[invalid_value]: refusing to overwrite `notes.txt`: it is not a SonicCraft output\n"
        );
    }

    #[test]
    fn tabs_wide_chars_and_long_lines() {
        let text = "a:\t\"日本語\" x\n";
        let rendered = render(&diag("x", 1, 4), Some(&view(text)), false, false);
        // `a:` + tab (4 cols) = 6 columns before the quote; the token is 2 quotes + 3 wide chars = 8 cols.
        assert!(rendered.contains("1 | a:    \"日本語\" x\n"), "{rendered}");
        assert!(rendered.contains("  |       ^^^^^^^^\n"), "{rendered}");

        let long = format!("text: \"{}\" bad {}\n", "w".repeat(150), "z".repeat(80));
        let col = 6 + 150 + 2 + 2; // 1-based column of `bad`
        let rendered = render(&diag("x", 1, col as u32), Some(&view(&long)), false, false);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[3].starts_with("1 | ...w"), "{rendered}");
        assert!(lines[3].ends_with("..."), "{rendered}");
        let code_start = lines[3].find("...").unwrap_or(0);
        let bad_at = lines[3].find(" bad ").unwrap_or(0) + 1;
        let caret_at = lines[4].find('^').unwrap_or(0);
        assert_eq!(caret_at, bad_at, "{rendered}");
        assert!(code_start > 0);
        assert!(lines[4].ends_with("^^^"), "{rendered}");
    }

    #[test]
    fn colors_wrap_parts() {
        let rendered = render(&diag("x", 3, 11), Some(&view(YAML)), true, false);
        assert!(
            rendered.starts_with("\x1b[1;31merror[x]\x1b[0m\x1b[1m: something is wrong\x1b[0m\n")
        );
        assert!(rendered.contains("\x1b[1;34m-->\x1b[0m timeline.yaml:3:11"));
        assert!(rendered.contains("\x1b[1;31m^^^^^\x1b[0m"));
    }

    #[test]
    fn filter_and_counts() {
        let diags = vec![
            Diagnostic::error("a", "e"),
            Diagnostic::warning("b", "w"),
            Diagnostic::info("c", "i"),
            Diagnostic::info("d", "i"),
        ];
        let normal = render_all(
            &diags,
            None,
            false,
            Filter {
                warnings: true,
                infos: false,
                paths: false,
            },
        );
        assert_eq!((normal.errors, normal.warnings, normal.infos), (1, 1, 0));
        assert_eq!(normal.hidden_infos, 2);
        assert_eq!(normal.counts(), "1 error, 1 warning");
        assert_eq!(normal.hidden_note(), " (2 notes hidden, use -v to show)");
        assert_eq!(normal.text.matches('\n').count(), 4);

        let verbose = render_all(
            &diags,
            None,
            false,
            Filter {
                warnings: true,
                infos: true,
                paths: true,
            },
        );
        assert_eq!(verbose.counts(), "1 error, 1 warning, 2 notes");
        let quiet = render_all(
            &diags,
            None,
            false,
            Filter {
                warnings: false,
                infos: false,
                paths: false,
            },
        );
        assert_eq!((quiet.errors, quiet.hidden_warnings), (1, 1));
        assert_eq!(counts_phrase(0, 0, 0), "no problems");
    }

    #[test]
    fn diagnostics_outside_the_document_have_no_location() {
        let diags = vec![
            Diagnostic::error(
                "format_requires_ffmpeg",
                "the requested format `opus` needs ffmpeg",
            )
            .with_path("output.format")
            .at(3, 11),
            Diagnostic::error("invalid_value", "unknown output format `flack`")
                .with_path("output.format")
                .at(3, 11),
        ];
        let filter = Filter {
            warnings: true,
            infos: false,
            paths: false,
        };
        let shown = render_all_with(&diags, Some(&view(YAML)), false, filter, &|d| {
            d.code != "format_requires_ffmpeg"
        });
        assert_eq!(shown.errors, 2);
        let blocks: Vec<&str> = shown.text.split("\n\n").collect();
        assert_eq!(
            blocks[0],
            "error[format_requires_ffmpeg]: the requested format `opus` needs ffmpeg"
        );
        assert!(
            blocks[1].contains(" --> timeline.yaml:3:11\n"),
            "{}",
            shown.text
        );
    }

    #[test]
    fn multi_line_hint() {
        let d = Diagnostic::warning("x", "m").with_hint("first\nsecond");
        assert_eq!(
            render(&d, None, false, false),
            format!("warning[x]: m\n  = hint: first\n{}second\n", " ".repeat(10))
        );
    }

    /// The snippet and caret lines of a rendered diagnostic.
    fn snippet(rendered: &str) -> (String, String) {
        let lines: Vec<&str> = rendered.lines().collect();
        (lines[3].to_string(), lines[4].to_string())
    }

    /// 1-based column of the first `needle` on 1-based line `line` of `text`.
    fn column_of(text: &str, line: usize, needle: &str) -> u32 {
        let l = text.lines().nth(line - 1).unwrap_or_default();
        let byte = l
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not on line {line}"));
        l[..byte].chars().count() as u32 + 1
    }

    const SECRETS: &str = "version: \"1.0\"\nproviders:\n  eleven:\n    type: elevenlabs\n    api_key: \"sk_live_0123456789abcdefABCDEF\"\n  oa: {type: openai, api_key: sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789, bogus: 1}\nx-notes:\n  password: hunter2hunter2\n  sk-abcdef0123456789abcdef0123456789: 1\n  token: |\n    tok_0123456789abcdef\n    second-line-of-it\ntracks: []\n";

    /// Fragments of the secrets in [`SECRETS`].
    const LEAKS: [&str; 6] = [
        "sk_live",
        "sk-proj",
        "hunter2",
        "sk-abcdef",
        "tok_0123",
        "second-line",
    ];

    fn assert_no_leak(rendered: &str) {
        for leak in LEAKS {
            assert!(!rendered.contains(leak), "`{leak}` leaked:\n{rendered}");
        }
    }

    #[test]
    fn quoted_secrets_keep_their_quotes_and_the_caret_follows() {
        let v = view(SECRETS);
        let d = Diagnostic::error(codes::SECRET_IN_TIMELINE, "`api_key` holds a secret")
            .at(5, column_of(SECRETS, 5, "\"sk_"));
        let rendered = render(&d, Some(&v), false, false);
        assert_no_leak(&rendered);
        let (line, caret) = snippet(&rendered);
        assert_eq!(line, "5 |     api_key: \"<redacted>\"", "{rendered}");
        assert_eq!(
            caret_span(&caret),
            (line.find('"').unwrap_or(0), 12),
            "{rendered}"
        );
        // The location still names the source position.
        assert!(rendered.contains(" --> timeline.yaml:5:14\n"), "{rendered}");
    }

    #[test]
    fn a_caret_after_a_secret_moves_with_the_replacement() {
        let v = view(SECRETS);
        // Any diagnostic on a line with a secret: the snippet is masked and the caret still sits under
        // its token.
        let d = Diagnostic::error("unknown_field", "unknown field `bogus`")
            .at(6, column_of(SECRETS, 6, "bogus"));
        let rendered = render(&d, Some(&v), false, false);
        assert_no_leak(&rendered);
        let (line, caret) = snippet(&rendered);
        assert_eq!(
            line, "6 |   oa: {type: openai, api_key: <redacted>, bogus: 1}",
            "{rendered}"
        );
        assert_eq!(
            caret_span(&caret),
            (line.find("bogus").unwrap_or(0), 5),
            "{rendered}"
        );
    }

    #[test]
    fn plain_values_keys_and_block_scalars_are_redacted() {
        let v = view(SECRETS);
        let line = |n: usize, needle: &str| {
            let d = Diagnostic::warning("x", "m").at(n as u32, column_of(SECRETS, n, needle));
            let rendered = render(&d, Some(&v), false, false);
            assert_no_leak(&rendered);
            snippet(&rendered).0
        };
        assert_eq!(line(8, "password"), "8 |   password: <redacted>");
        // A key shaped like an API key.
        assert_eq!(line(9, ": 1"), "9 |   <redacted>: 1");
        // Every content line of a block scalar; the indentation stays.
        assert_eq!(line(11, "tok"), "11 |     <redacted>");
        assert_eq!(line(12, "second"), "12 |     <redacted>");
        // Lines without secrets are untouched.
        assert_eq!(line(4, "elevenlabs"), "4 |     type: elevenlabs");
    }

    #[test]
    fn the_caret_token_of_a_secret_diagnostic_is_always_redacted() {
        // Nothing here is secret-shaped for the scan, but the diagnostic says the token is a secret.
        let text = "version: \"1.0\"\nnote: hunter2hunter2 # x\n";
        let d = Diagnostic::error(codes::SECRET_IN_TIMELINE, "`note` holds a secret").at(2, 7);
        let rendered = render(&d, Some(&view(text)), false, false);
        let (line, caret) = snippet(&rendered);
        assert_eq!(line, "2 | note: <redacted> # x", "{rendered}");
        assert_eq!(caret_span(&caret), (line.find('<').unwrap_or(0), 10));
        // Other diagnostics show such a line as written.
        let d = Diagnostic::warning("x", "m").at(2, 7);
        let rendered = render(&d, Some(&view(text)), false, false);
        assert!(rendered.contains("note: hunter2hunter2 # x"), "{rendered}");
    }

    #[test]
    fn json_bom_crlf_and_long_lines_are_masked() {
        let json = "\u{feff}{\r\n  \"providers\": {\"e\": {\"type\": \"elevenlabs\", \"api_key\": \"sk_live_0123456789abcdefABCDEF\"}},\r\n  \"tracks\": []\r\n}\r\n";
        let v = SourceView::new("t.json", json, SourceFormat::Json);
        let col = column_of(json, 2, "\"sk_");
        let rendered = render(
            &Diagnostic::warning("x", "m").at(2, col),
            Some(&v),
            false,
            false,
        );
        assert_no_leak(&rendered);
        let (line, caret) = snippet(&rendered);
        assert!(
            line.ends_with("\"api_key\": \"<redacted>\"}},"),
            "{rendered}"
        );
        assert_eq!(caret_span(&caret), (line.find("\"<").unwrap_or(0), 12));

        // A secret far into a long line: the window is cut around the masked line.
        let long = format!(
            "text: \"{}\"\nx: {{a: {}, api_key: \"sk_live_0123456789abcdefABCDEF\", b: {}, bogus: 1}}\n",
            "w".repeat(10),
            "1".repeat(90),
            "2".repeat(90)
        );
        let col = column_of(&long, 2, "bogus");
        let rendered = render(
            &Diagnostic::warning("x", "m").at(2, col),
            Some(&view(&long)),
            false,
            false,
        );
        assert_no_leak(&rendered);
        let (line, caret) = snippet(&rendered);
        assert_eq!(
            caret_span(&caret).0,
            line.find("bogus").unwrap_or(0),
            "{rendered}"
        );
        let col = column_of(&long, 2, "\"sk_");
        let rendered = render(
            &Diagnostic::warning("x", "m").at(2, col),
            Some(&view(&long)),
            false,
            false,
        );
        assert_no_leak(&rendered);
        let (line, caret) = snippet(&rendered);
        assert!(line.contains("api_key: \"<redacted>\""), "{rendered}");
        assert_eq!(caret_span(&caret), (line.find("\"<").unwrap_or(0), 12));
    }

    /// The snippet algorithm before secrets and long-line regions (whole line, rescanned per call): the
    /// reference that `excerpt` must match on lines without secrets.
    fn reference_excerpt(line: &str, col: Option<usize>, json: bool) -> Excerpt {
        fn in_flow_context(chars: &[char], pos: usize) -> bool {
            let (mut depth, mut quote, mut i) = (0usize, None::<char>, 0);
            while i < pos.min(chars.len()) {
                let c = chars[i];
                match quote {
                    Some('"') if c == '\\' => i += 1,
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None => match c {
                        '"' | '\'' => quote = Some(c),
                        '#' if i == 0 || chars[i - 1].is_whitespace() => return depth > 0,
                        '{' | '[' => depth += 1,
                        '}' | ']' => depth = depth.saturating_sub(1),
                        _ => {}
                    },
                }
                i += 1;
            }
            depth > 0
        }
        let chars: Vec<char> = line.chars().collect();
        let start = col.map(|c| c.min(chars.len()));
        let len = start.map(|s| token_len(&chars, s, json || in_flow_context(&chars, s)));
        let widths: Vec<usize> = chars
            .iter()
            .map(|&c| if c == '\t' { 4 } else { char_width(c) })
            .collect();
        let (mut from, mut to) = (0, chars.len());
        if widths.iter().sum::<usize>() > MAX_LINE_COLS {
            let anchor = start.unwrap_or(0);
            let mut cols = 0;
            from = anchor;
            while from > 0 && cols + widths[from - 1] <= LEAD_COLS {
                from -= 1;
                cols += widths[from];
            }
            let mut used: usize = widths[from..anchor.min(chars.len())].iter().sum();
            to = anchor.min(chars.len());
            while to < chars.len() && used + widths[to] <= MAX_LINE_COLS {
                used += widths[to];
                to += 1;
            }
        }
        let mut text = String::new();
        let prefix_cols = if from > 0 { 3 } else { 0 };
        if from > 0 {
            text.push_str("...");
        }
        for &c in &chars[from..to] {
            if c == '\t' {
                text.push_str("    ");
            } else {
                text.push(c);
            }
        }
        if to < chars.len() {
            text.push_str("...");
        }
        let caret = start.map(|s| {
            let s = s.max(from);
            let offset = prefix_cols + widths[from..s.min(to)].iter().sum::<usize>();
            let end = (s + len.unwrap_or(1)).min(to);
            (offset, widths[s.min(to)..end].iter().sum::<usize>().max(1))
        });
        Excerpt {
            text: text.trim_end().to_string(),
            caret,
        }
    }

    #[test]
    fn excerpts_match_the_reference_on_random_lines() {
        const ALPHABET: [char; 22] = [
            'a', 'b', 'z', '0', ' ', ' ', ' ', '\t', ':', ',', '#', '{', '}', '[', ']', '"', '\'',
            '\\', '-', 'é', '日', '😀',
        ];
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for case in 0..3000 {
            let len = match case % 4 {
                0 => next(40),
                1 => next(200),
                2 => 590 + next(30),
                _ => 600 + next(1500),
            };
            let line: String = (0..len).map(|_| ALPHABET[next(ALPHABET.len())]).collect();
            let col = (case % 7 != 0).then(|| next(len + 3));
            let json = case % 3 == 0;
            let prepared = Prepared::new(&line, &[], json);
            assert_eq!(
                excerpt(&prepared, col, false),
                reference_excerpt(&line, col, json),
                "case {case}: {line:?} col {col:?} json {json}"
            );
        }
    }

    #[test]
    fn long_lines_are_prepared_once_and_rendered_near_the_caret() {
        // A minified 1 MB document with a diagnostic every 100 chars: rescanning the line per diagnostic
        // took about a minute; now each one looks at a few hundred chars.
        let line = format!(
            "{{\"tracks\": [{}]}}",
            vec!["{\"id\": \"c\", \"bogus\": 1, \"text\": \"some words to speak\"}"; 18_000]
                .join(", ")
        );
        let text = format!("{line}\n");
        let v = SourceView::new("t.json", &text, SourceFormat::Json);
        let started = std::time::Instant::now();
        let mut col = 13;
        let mut rendered = 0;
        while col + 200 < line.len() as u32 {
            let d = Diagnostic::error("unknown_field", "unknown field `bogus`").at(1, col);
            let block = render(&d, Some(&v), false, false);
            assert!(block.contains("\"bogus\": 1"), "{block}");
            col += 100;
            rendered += 1;
        }
        assert!(rendered > 10_000);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{rendered} snippets took {:?}",
            started.elapsed()
        );
        let first = v.prepare(1).expect("line 1");
        let again = v.prepare(1).expect("line 1");
        assert!(Rc::ptr_eq(&first, &again), "long lines are cached");
    }

    #[test]
    fn masking_maps_positions() {
        let line: Vec<char> = "ab \"secret\" cd sk-x ef".chars().collect();
        let secrets = [
            Secret {
                chars: 3..11,
                whole: true,
            },
            Secret {
                chars: 15..19,
                whole: true,
            },
        ];
        let m = Masked::new(&line, &secrets);
        let shown: String = m.chars.iter().collect();
        assert_eq!(shown, "ab \"<redacted>\" cd <redacted> ef");
        assert_eq!(m.map(0, false), 0);
        assert_eq!(m.map(3, false), 3); // the opening quote
        assert_eq!(m.map(5, false), 3); // inside: start of the replacement
        assert_eq!(m.map(5, true), 15); // inside: end of the replacement
        assert_eq!(m.map(11, false), 15); // just after the closing quote
        assert_eq!(m.map(12, false), 16); // `cd`
        assert_eq!(m.map(20, false), shown.find("ef").unwrap_or(0));
        assert_eq!(m.map(line.len(), true), shown.chars().count());
        // A range that does not start and end on this line, or is not quoted on both sides, loses its
        // quotes; whitespace at its ends is kept.
        let open = Masked::new(
            &line,
            &[Secret {
                chars: 2..8,
                whole: false,
            }],
        );
        assert_eq!(
            open.chars.iter().collect::<String>(),
            "ab <redacted>et\" cd sk-x ef"
        );
    }
}
