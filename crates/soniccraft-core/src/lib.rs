//! SonicCraft core: the timeline DSL (raw AST, parsing, validation/lowering, JSON Schema) and the IR that
//! the rest of the engine consumes.
//!
//! Pipeline: [`parse::parse_timeline`] (YAML/JSON → raw [`model::Timeline`] + diagnostics + source spans)
//! → [`lower::lower`] (raw AST → validated [`ir::Program`] + diagnostics). [`compile`] runs both and
//! attaches line/column information to every diagnostic.
//!
//! This crate performs no I/O beyond reading environment variables; it is pure and deterministic.

#![warn(missing_docs)]

pub mod diagnostics;
mod fields;
pub mod ir;
pub mod lower;
pub mod model;
pub mod parse;
pub mod providers;
pub mod schema;
pub mod values;

use std::collections::HashMap;

pub use diagnostics::{Diagnostic, Severity};
pub use ir::Program;
pub use lower::{AssetRef, LowerOptions, Lowered};
pub use parse::{Parsed, SourceFormat, SpanIndex};

/// The DSL version this crate implements.
pub const DSL_VERSION: &str = "1.0";

/// The largest time value (and the longest timeline) SonicCraft accepts: 24 hours, in milliseconds.
/// Offsets may be negative down to `-MAX_TIME_MS`.
pub const MAX_TIME_MS: f64 = 86_400_000.0;

/// Result of [`compile`]. Callers that build one themselves (e.g. for an internal failure) should write
/// `Compiled { diagnostics, ..Compiled::default() }`, so fields added later do not break them.
#[derive(Debug, Clone, Default)]
pub struct Compiled {
    /// The program, present only when there are no errors.
    pub program: Option<Program>,
    /// All diagnostics from parsing and lowering, with line/column attached where known, sorted.
    pub diagnostics: Vec<Diagnostic>,
    /// Source spans, for callers that add their own diagnostics later (engine environment checks).
    pub spans: SpanIndex,
    /// Every audio file the document names (each string `source` of a music/sfx track or clip), in document
    /// order, also when the document has errors and [`Compiled::program`] is None, so environment checks
    /// (`asset_not_found`, `asset_unreadable`) can run in the same pass. Empty when the document does not
    /// parse.
    pub assets: Vec<AssetRef>,
}

/// Parse and lower a timeline document in one step.
///
/// Lowering runs even when parsing reported errors, so every semantic problem surfaces in one pass;
/// lowering diagnostics that are mere consequences of a problem parsing already reported are left out: a
/// value dropped during parse recovery (at or below a path in [`Parsed::dropped`], an anchor whose only
/// target was dropped, a loop whose length was dropped), a misspelled key the did-you-mean names, and a
/// sequence item made only of misspelled pause keys (`{pause_ms: 300}` is lowered as a pause, not as a cue
/// without text). Secret values found in the document are redacted from every message. A program is
/// returned only when the whole document is error-free.
pub fn compile(text: &str, format: SourceFormat, options: &LowerOptions) -> Compiled {
    let parsed = parse::parse_timeline(text, format);
    let mut diagnostics = parsed.diagnostics.clone();
    let assets = parsed
        .timeline
        .as_ref()
        .map(|timeline| lower::assets::collect(timeline, &parsed.spans))
        .unwrap_or_default();
    let program = parsed.timeline.as_ref().and_then(|timeline| {
        let unknown = UnknownFields::new(&parsed.diagnostics);
        let misspelled_pauses = misspelled_pauses(parsed.value.as_ref(), &unknown);
        let recovery = lower::Recovery {
            dropped: &parsed.dropped,
            misspelled_pauses: &misspelled_pauses,
        };
        let lowered = lower::lower_parsed(timeline, options, recovery);
        let dropped = lower::DroppedPaths::new(&parsed.dropped);
        let mut extra: Vec<Diagnostic> = lowered
            .diagnostics
            .into_iter()
            .filter(|d| !d.path.as_deref().is_some_and(|p| dropped.covers(p)))
            .filter(|d| !superseded_by_unknown_field(d, &unknown))
            .collect();
        if let Some(value) = &parsed.value {
            parse::secrets::redact(&mut extra, &parse::secrets::scan(value).values);
        }
        diagnostics.extend(extra);
        lowered.program
    });
    for d in &mut diagnostics {
        parsed.spans.attach(d);
    }
    diagnostics::sort(&mut diagnostics);
    let program = if diagnostics::has_errors(&diagnostics) {
        None
    } else {
        program
    };
    Compiled {
        program,
        diagnostics,
        spans: parsed.spans,
        assets,
    }
}

/// The `unknown_field` diagnostics of the parse, by the path of the object that has the unknown key
/// (`tracks[0].sequence[1]` for `tracks[0].sequence[1].txt`), so the checks below cost O(siblings) per
/// lookup instead of O(every unknown key in the document).
struct UnknownFields<'d> {
    by_parent: HashMap<&'d str, Vec<&'d Diagnostic>>,
}

impl<'d> UnknownFields<'d> {
    fn new(diagnostics: &'d [Diagnostic]) -> Self {
        let mut by_parent: HashMap<&'d str, Vec<&'d Diagnostic>> = HashMap::new();
        for d in diagnostics {
            if d.code == diagnostics::codes::UNKNOWN_FIELD
                && let Some(path) = d.path.as_deref()
            {
                by_parent.entry(parent_path(path)).or_default().push(d);
            }
        }
        UnknownFields { by_parent }
    }

    /// The unknown keys of the object at `parent`.
    fn of(&self, parent: &str) -> &[&'d Diagnostic] {
        self.by_parent.get(parent).map_or(&[], Vec::as_slice)
    }
}

/// `tracks[0].sequence` for `tracks[0].sequence.text` (the empty path for a top-level key).
fn parent_path(path: &str) -> &str {
    path.rfind('.').map_or("", |i| &path[..i])
}

/// Paths of the sequence items whose only keys (annotations aside) are pause-like (`{pause_ms: 300}`,
/// `{silence: 1s}`, `{wait: 1s}`, or a key the parser's did-you-mean maps to `pause` / `pause_duration`)
/// and at least one of them unknown. Such an item is a misspelled pause, which parsing reported as
/// `unknown_field` with a hint; lowering it as a cue would add "no `text`" (and provider or voice) errors
/// whose hints turn the pause into speech.
fn misspelled_pauses(
    value: Option<&serde_json::Value>,
    unknown: &UnknownFields<'_>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(tracks) = value
        .and_then(|v| v.get("tracks"))
        .and_then(serde_json::Value::as_array)
    else {
        return out;
    };
    for (i, track) in tracks.iter().enumerate() {
        let Some(sequence) = track.get("sequence").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for (j, item) in sequence.iter().enumerate() {
            let Some(item) = item.as_object() else {
                continue;
            };
            let path = format!("tracks[{i}].sequence[{j}]");
            let siblings = unknown.of(&path);
            if siblings.is_empty() {
                // Every key is a known field: a pause (`pause`, `pause_duration`) is already one.
                continue;
            }
            let hinted_pause = |key: &str| {
                siblings.iter().any(|u| {
                    u.path
                        .as_deref()
                        .and_then(|p| p.strip_prefix(path.as_str()))
                        .and_then(|rest| rest.strip_prefix('.'))
                        == Some(key)
                        && u.hint.as_deref().is_some_and(|h| {
                            h.starts_with("Did you mean `pause`")
                                || h.starts_with("Did you mean `pause_duration`")
                        })
                })
            };
            let mut keys = item
                .keys()
                .filter(|k| !k.starts_with("x-") && !k.starts_with("x_"))
                .peekable();
            if keys.peek().is_some() && keys.all(|k| is_pause_like_key(k) || hinted_pause(k)) {
                out.push(path);
            }
        }
    }
    out
}

/// Keys agents write for a pause: `pause_ms`, `silence`, `wait`, `delay_ms`, `gap`, `break`, … (any of
/// `pause`/`silence`/`wait`/`delay`/`gap`/`sleep`/`rest`/`break` with an optional unit or `_duration` /
/// `_length` / `_time` suffix). The valid keys `pause` and `pause_duration` match too, which is harmless:
/// an item with them is a pause anyway.
fn is_pause_like_key(key: &str) -> bool {
    let k = diagnostics::normalize_key(key).to_ascii_lowercase();
    let mut stem = k.as_str();
    for suffix in [
        "_milliseconds",
        "_millis",
        "_msec",
        "_ms",
        "_seconds",
        "_secs",
        "_sec",
        "_s",
    ] {
        if let Some(s) = stem.strip_suffix(suffix) {
            stem = s;
            break;
        }
    }
    for suffix in ["_duration", "_length", "_time"] {
        if let Some(s) = stem.strip_suffix(suffix) {
            stem = s;
            break;
        }
    }
    matches!(
        stem,
        "pause" | "silence" | "wait" | "delay" | "gap" | "sleep" | "rest" | "break"
    )
}

/// True for a lowering complaint about a missing key (or an anchor without a target, or no tracks) whose
/// real cause is a misspelled key that parsing already reported with a did-you-mean hint naming that key
/// (`txt:` → "unknown field `txt` … Did you mean `text`?" makes "cue has no `text`" redundant).
fn superseded_by_unknown_field(d: &Diagnostic, unknown: &UnknownFields<'_>) -> bool {
    use diagnostics::codes;
    let Some(path) = d.path.as_deref() else {
        return false;
    };
    let names_field = |u: &Diagnostic, field: &str| {
        u.hint
            .as_deref()
            .is_some_and(|h| h.contains(&format!("`{field}`")))
    };
    match d.code.as_str() {
        codes::MISSING_FIELD | codes::EMPTY_TRACKS => {
            let field = path.rsplit('.').next().unwrap_or(path);
            unknown
                .of(parent_path(path))
                .iter()
                .any(|u| names_field(u, field))
        }
        codes::CONFLICTING_FIELDS if d.message.contains("needs exactly one of") => {
            unknown.of(path).iter().any(|u| {
                ["after_cue", "with_cue", "before_cue"]
                    .iter()
                    .any(|f| names_field(u, f))
            })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_values_do_not_cascade() {
        // `id: 7` is an invalid_type; lowering would also report a missing id at the same path.
        let text = "version: \"1.0\"\ntracks:\n  - id: 7\n    type: speech\n    sequence:\n      - text: hi\n        provider: mock\n";
        let c = compile(text, SourceFormat::Yaml, &LowerOptions::default());
        let codes: Vec<&str> = c
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.code.as_str())
            .collect();
        assert_eq!(codes, ["invalid_type"], "{:?}", c.diagnostics);
        assert!(c.program.is_none());
    }

    #[test]
    fn misspelled_keys_do_not_cascade() {
        let text = "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - txt: hi\n        provider: mock\n      - text: b\n        provider: mock\n        start_relative: {after: v.cue1}\n";
        let c = compile(text, SourceFormat::Yaml, &LowerOptions::default());
        let got: Vec<(&str, &str)> = c
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(|d| (d.code.as_str(), d.path.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            got,
            [
                ("unknown_field", "tracks[0].sequence[0].txt"),
                (
                    "unknown_field",
                    "tracks[0].sequence[1].start_relative.after"
                )
            ],
            "{:?}",
            c.diagnostics
        );
    }

    #[test]
    fn pause_like_keys() {
        for key in [
            "pause_ms",
            "Pause",
            "pauseMs",
            "silence",
            "silence_ms",
            "wait",
            "delay_ms",
            "gap",
            "break",
            "sleep_s",
            "rest",
            "pause_length",
            "pause_duration_ms",
            "pause",
        ] {
            assert!(is_pause_like_key(key), "{key}");
        }
        for key in [
            "text",
            "pauser",
            "speaker",
            "silent",
            "duration_ms",
            "start_ms",
            "waiting",
        ] {
            assert!(!is_pause_like_key(key), "{key}");
        }
    }
}
