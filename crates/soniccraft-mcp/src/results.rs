//! Assembling `tools/call` results.
//!
//! Every result carries the report as `structuredContent` (a JSON object), a compact summary as the first
//! text block and the same JSON as the second text block for clients that ignore `structuredContent`.
//! Render results add `resource_link` blocks (`file://` URIs, absolute paths) for the audio and every side
//! file; audio is never inlined.
//!
//! # Size
//!
//! Results are sized for an agent's context, not only for the transport: Claude Code, for one, keeps a
//! tool result out of the conversation when it exceeds about 25,000 tokens (50,000 characters).
//!
//! * `detail: "auto"` (the default) returns the full report while its JSON has at most
//!   [`AUTO_REPORT_LIMIT`] bytes and the compact report ([`crate::compact`]) above that;
//!   `detail: "summary"` always returns the compact report. Either way the whole result (every content
//!   block plus `structuredContent`, as JSON) stays within [`AGENT_RESULT_BUDGET`] bytes.
//! * `detail: "full"` returns everything that fits in one message ([`MESSAGE_RESULT_BUDGET`], below the
//!   transport's limit).
//!
//! A result that does not fit is shortened step by step, each step smaller than the last ([`Level`]): the
//! JSON mirror is replaced by a note, then (for `full`) the diagram and the cue texts are dropped, then
//! the lists are capped ever more tightly, with counts and a `full_report` note saying where everything
//! is. A valid report therefore never becomes a JSON-RPC error.
//!
//! # Protocol revisions
//!
//! `resource_link` content arrived in MCP 2025-06-18; sessions on an older revision (2024-11-05,
//! 2025-03-26), whose clients may reject unknown content types, get the files as a text block instead.

use std::cell::OnceCell;
use std::path::Path;

use rmcp::model::{CallToolResult, ContentBlock, ProtocolVersion, Resource};
use serde::Serialize;
use serde_json::{Value, json};
use soniccraft_engine::{
    PlanReport, ProvidersReport, RenderFailure, RenderReport, ValidationReport, VoicesReport,
};

use crate::args::Detail;
use crate::compact::{self, Caps, Labels};
use crate::paths;
use crate::summary::{self, Rest, SummaryCaps};

/// With `detail: "auto"`, a report whose JSON is longer than this (in bytes) is returned in compact form.
pub(crate) const AUTO_REPORT_LIMIT: usize = 12_000;

/// With `detail: "auto"` or `"summary"`, the whole result stays within this many bytes of JSON (every
/// content block plus `structuredContent`; non-ASCII text counts more than once, so the limit is
/// conservative in characters).
pub(crate) const AGENT_RESULT_BUDGET: usize = 40_000;

/// Every result stays within this many bytes of JSON, well below the transport's 7 MiB message cap
/// ([`crate::transport::MAX_OUTPUT_LINE_BYTES`]) and Codex's 8 MiB line limit.
pub(crate) const MESSAGE_RESULT_BUDGET: usize = 6 * 1024 * 1024;

/// Most resource links (or file lines) in a render result; the report lists every file.
const MAX_FILE_LINKS: usize = 50;

/// What the client's protocol revision allows in a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Era {
    /// `resource_link` content blocks (MCP 2025-06-18 and later).
    pub(crate) resource_links: bool,
}

impl Era {
    /// The era of a request's protocol version (the negotiated one, or the request's own `_meta` in the
    /// 2026-07-28 revision). An unknown version gets only what every revision defines.
    pub(crate) fn of(version: Option<&ProtocolVersion>) -> Era {
        Era {
            // ISO dates compare lexically the same as chronologically.
            resource_links: version.is_some_and(|v| v.as_str() >= "2025-06-18"),
        }
    }
}

/// How one result is shaped: the `detail` argument and the client's era.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) detail: Detail,
    pub(crate) era: Era,
}

/// One step of the shortening sequence.
#[derive(Debug, Clone, Copy)]
enum Level {
    /// The full report; `mirror` repeats it as JSON text.
    Full { mirror: bool, summary: SummaryCaps },
    /// The full report without the plan diagram and, unless `texts`, without the cue texts.
    Lean { texts: bool },
    /// The compact report with `caps`.
    Compact {
        caps: Caps,
        mirror: bool,
        summary: SummaryCaps,
    },
    /// Only the headline facts.
    Minimal,
}

/// `detail: "full"`: everything, shortened only to fit one message.
const FULL_LEVELS: &[Level] = &[
    Level::Full {
        mirror: true,
        summary: SummaryCaps::STANDARD,
    },
    Level::Full {
        mirror: false,
        summary: SummaryCaps::STANDARD,
    },
    Level::Lean { texts: true },
    Level::Lean { texts: false },
    Level::Compact {
        caps: Caps::LARGE,
        mirror: false,
        summary: SummaryCaps::STANDARD,
    },
    Level::Compact {
        caps: Caps::MEDIUM,
        mirror: false,
        summary: SummaryCaps::STANDARD,
    },
    Level::Compact {
        caps: Caps::STANDARD,
        mirror: false,
        summary: SummaryCaps::SHORT,
    },
    Level::Compact {
        caps: Caps::TINY,
        mirror: false,
        summary: SummaryCaps::TINY,
    },
    Level::Minimal,
];

/// `detail: "auto"` with a small report: the full report, then the compact one if even that does not fit.
const AUTO_SMALL_LEVELS: &[Level] = &[
    Level::Full {
        mirror: true,
        summary: SummaryCaps::STANDARD,
    },
    Level::Full {
        mirror: false,
        summary: SummaryCaps::STANDARD,
    },
    Level::Compact {
        caps: Caps::STANDARD,
        mirror: false,
        summary: SummaryCaps::SHORT,
    },
    Level::Compact {
        caps: Caps::REDUCED,
        mirror: false,
        summary: SummaryCaps::SHORT,
    },
    Level::Compact {
        caps: Caps::TINY,
        mirror: false,
        summary: SummaryCaps::TINY,
    },
    Level::Minimal,
];

/// `detail: "summary"`, and `"auto"` with a large report.
const COMPACT_LEVELS: &[Level] = &[
    Level::Compact {
        caps: Caps::STANDARD,
        mirror: true,
        summary: SummaryCaps::STANDARD,
    },
    Level::Compact {
        caps: Caps::STANDARD,
        mirror: false,
        summary: SummaryCaps::STANDARD,
    },
    Level::Compact {
        caps: Caps::REDUCED,
        mirror: false,
        summary: SummaryCaps::SHORT,
    },
    Level::Compact {
        caps: Caps::TINY,
        mirror: false,
        summary: SummaryCaps::TINY,
    },
    Level::Minimal,
];

/// A report the builder can shorten.
#[derive(Clone, Copy)]
enum Report<'a> {
    Validation(&'a ValidationReport, &'a str),
    Plan(&'a PlanReport, &'a str),
    Render(&'a RenderReport),
    RenderFailure(&'a RenderFailure, &'a str),
    Voices(&'a VoicesReport),
    Providers(&'a ProvidersReport),
}

impl Report<'_> {
    fn full(&self) -> Value {
        match self {
            Report::Validation(r, _) => to_value(*r),
            Report::Plan(r, _) => to_value(*r),
            Report::Render(r) => to_value(*r),
            Report::RenderFailure(r, _) => to_value(*r),
            Report::Voices(r) => to_value(*r),
            Report::Providers(r) => to_value(*r),
        }
    }

    /// Length of the full report's JSON, without building it.
    fn full_len(&self) -> usize {
        match self {
            Report::Validation(r, _) => json_len(*r),
            Report::Plan(r, _) => json_len(*r),
            Report::Render(r) => json_len(*r),
            Report::RenderFailure(r, _) => json_len(*r),
            Report::Voices(r) => json_len(*r),
            Report::Providers(r) => json_len(*r),
        }
    }

    /// The full report without the plan diagram and, unless `texts`, without the cue texts (still every
    /// item and diagnostic), with a note on what was left out. Unchanged when there is nothing to leave out.
    fn lean(&self, texts: bool) -> Value {
        let mut value = self.full();
        let mut left_out = Vec::new();
        if let Some(diagram) = value.get_mut("diagram")
            && diagram.as_str().is_some_and(|d| !d.is_empty())
        {
            *diagram = json!("");
            value["diagram_truncated"] = json!(true);
            left_out.push("diagram");
        }
        if !texts && let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
            for item in items.iter_mut().filter_map(Value::as_object_mut) {
                item.remove("text");
            }
            left_out.push("cue texts");
        }
        if !left_out.is_empty()
            && let Some(object) = value.as_object_mut()
        {
            object.insert("detail".into(), json!("full"));
            object.insert(
                "full_report".into(),
                json!(format!(
                    "The {} {} left out to fit one MCP message; {}",
                    left_out.join(" and the "),
                    if left_out.len() == 1 { "was" } else { "were" },
                    self.complete_report()
                )),
            );
        }
        value
    }

    /// Where the complete report is when it cannot be sent.
    fn complete_report(&self) -> String {
        match self {
            Report::Validation(..) | Report::RenderFailure(..) => {
                "`soniccraft validate --json` lists every diagnostic".into()
            }
            Report::Plan(..) => "`soniccraft plan --json` has the complete report".into(),
            Report::Render(r) => r.files.manifest.as_ref().map_or_else(
                || {
                    "set `output.export_manifest: true` to write every item to a manifest file"
                        .into()
                },
                |path| format!("the manifest {path} has every item"),
            ),
            Report::Voices(r) => format!(
                "`soniccraft voices {} --json` lists every voice",
                r.provider
            ),
            Report::Providers(_) => "`soniccraft providers --json` has the complete report".into(),
        }
    }

    fn compact(&self, caps: &Caps, labels: &Labels) -> Value {
        match self {
            Report::Validation(r, _) => compact::validation(r, caps, labels),
            Report::Plan(r, _) => compact::plan(r, caps, labels),
            Report::Render(r) => compact::render(r, caps, labels),
            Report::RenderFailure(r, _) => compact::render_failure(r, caps, labels),
            Report::Voices(r) => compact::voices(r, caps, labels),
            // Eight providers: nothing to cap.
            Report::Providers(r) => to_value(*r),
        }
    }

    /// Only the headline facts: totals, counts, the output file and its loudness.
    fn minimal(&self, labels: &Labels) -> Value {
        let caps = Caps {
            diagnostics: 3,
            edge_items: 0,
            ..Caps::TINY
        };
        match self {
            Report::Validation(r, _) => {
                let mut v = compact::validation(r, &caps, labels);
                drop_keys(&mut v, &["summary"]);
                v
            }
            Report::Plan(r, _) => {
                let mut v = compact::plan(r, &caps, labels);
                drop_keys(&mut v, &["tracks", "items", "ducking", "diagram"]);
                if let Some(synthesis) = v.get_mut("synthesis") {
                    drop_keys(synthesis, &["by_provider"]);
                }
                v
            }
            Report::Render(r) => {
                let mut v = compact::render(r, &caps, labels);
                drop_keys(&mut v, &["tracks", "items", "ducking", "timings_ms"]);
                if let Some(files) = v.get_mut("files") {
                    drop_keys(files, &["stems"]);
                }
                v
            }
            Report::RenderFailure(r, _) => compact::render_failure(r, &caps, labels),
            Report::Voices(r) => compact::voices(r, &caps, labels),
            Report::Providers(r) => to_value(*r),
        }
    }

    fn summary(&self, caps: &SummaryCaps) -> String {
        match self {
            Report::Validation(r, name) => summary::validation(r, name, caps),
            Report::Plan(r, name) => summary::plan(r, name, caps),
            Report::Render(r) => summary::render(r, caps),
            Report::RenderFailure(r, name) => summary::render_failure(r, name, caps),
            Report::Voices(r) => summary::voices(r),
            Report::Providers(r) => summary::providers(r),
        }
    }

    /// Where the complete report is, for a compact report (`too_large`: for a full report that does not
    /// fit in one message).
    fn full_report_note(&self, too_large: bool) -> String {
        let manifest = |r: &RenderReport| {
            r.files
                .manifest
                .as_ref()
                .map(|path| format!("the manifest {path} has every item"))
        };
        if too_large {
            return format!(
                "the complete report is larger than one MCP message allows; {}",
                self.complete_report()
            );
        }
        match self {
            Report::Validation(..) => {
                "call soniccraft_validate_timeline again with detail: \"full\", or run `soniccraft validate --json`"
                    .into()
            }
            Report::Plan(..) => {
                "call soniccraft_dry_run again with detail: \"full\", or run `soniccraft plan --json`".into()
            }
            Report::Render(r) => manifest(r).unwrap_or_else(|| {
                "call soniccraft_render again with detail: \"full\" (cached speech makes it quick), or set \
                 `output.export_manifest: true` to write every item to a manifest file"
                    .into()
            }),
            Report::RenderFailure(..) => {
                "call soniccraft_validate_timeline with detail: \"full\" to get every diagnostic".into()
            }
            Report::Voices(_) | Report::Providers(_) => self.complete_report(),
        }
    }

    /// The JSON mirror's note when the mirror is left out.
    fn mirror_note(&self, len: usize) -> String {
        format!(
            "The JSON report ({len} bytes) is not repeated as text to keep this result small; read it from \
             structuredContent."
        )
    }
}

fn to_value<T: Serialize>(report: &T) -> Value {
    serde_json::to_value(report).unwrap_or_else(|error| {
        // The engine's reports always serialize; never fail a call over it.
        json!({"error": format!("could not serialize the report: {error}")})
    })
}

fn drop_keys(value: &mut Value, keys: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        for key in keys {
            object.shift_remove(*key);
        }
    }
}

/// Counts the bytes written to it.
struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Length of `value` as compact JSON, without building the text.
fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut counter = Counter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => usize::MAX,
    }
}

/// Length of a result as it is sent (compact JSON).
pub(crate) fn result_len(result: &CallToolResult) -> usize {
    json_len(result)
}

/// Shape `report` into a result: pick the level sequence for `shape.detail` and return the first level
/// that fits its budget (the last level always fits).
fn build(
    report: Report<'_>,
    is_error: bool,
    extra: &[ContentBlock],
    shape: Shape,
) -> CallToolResult {
    let full_len = report.full_len();
    let (levels, budget) = match shape.detail {
        Detail::Full => (FULL_LEVELS, MESSAGE_RESULT_BUDGET),
        Detail::Auto if full_len <= AUTO_REPORT_LIMIT => (AUTO_SMALL_LEVELS, AGENT_RESULT_BUDGET),
        Detail::Auto | Detail::Summary => (COMPACT_LEVELS, AGENT_RESULT_BUDGET),
    };
    let full = OnceCell::new();
    let mut last = None;
    for level in levels {
        // A mirror can never fit when the report alone takes half the budget: skip building it.
        if let Level::Full { mirror: true, .. } = level
            && full_len.saturating_mul(2) > budget
        {
            continue;
        }
        let result = assemble(report, *level, is_error, extra, shape.detail, &full);
        if result_len(&result) <= budget {
            return result;
        }
        last = Some(result);
    }
    last.unwrap_or_else(|| assemble(report, Level::Minimal, is_error, extra, shape.detail, &full))
}

fn assemble(
    report: Report<'_>,
    level: Level,
    is_error: bool,
    extra: &[ContentBlock],
    detail: Detail,
    full: &OnceCell<Value>,
) -> CallToolResult {
    // A compact report is what `summary` asks for; in `full` it is a full report that did not fit.
    let too_large = detail == Detail::Full;
    let labels = || Labels {
        detail: if too_large { "full" } else { "summary" },
        full_report: report.full_report_note(too_large),
    };
    let rest = if too_large {
        Rest::TooLarge
    } else {
        Rest::AskFull
    };
    let (summary, structured, mirror) = match level {
        Level::Full { mirror, summary } => (
            report.summary(&summary),
            full.get_or_init(|| report.full()).clone(),
            mirror,
        ),
        Level::Lean { texts } => (
            report.summary(&SummaryCaps::STANDARD.with_rest(Rest::TooLarge)),
            report.lean(texts),
            false,
        ),
        Level::Compact {
            caps,
            mirror,
            summary,
        } => (
            report.summary(&summary.with_rest(rest)),
            report.compact(&caps, &labels()),
            mirror,
        ),
        Level::Minimal => (
            report.summary(&SummaryCaps::MINIMAL.with_rest(rest)),
            report.minimal(&labels()),
            false,
        ),
    };
    let second = if mirror {
        structured.to_string()
    } else {
        report.mirror_note(json_len(&structured))
    };
    let mut content = vec![ContentBlock::text(summary), ContentBlock::text(second)];
    content.extend(extra.iter().cloned());
    let mut result = if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    result.structured_content = Some(structured);
    result
}

/// The validate result.
pub(crate) fn validation(report: &ValidationReport, name: &str, shape: Shape) -> CallToolResult {
    build(Report::Validation(report, name), false, &[], shape)
}

/// The dry-run result.
pub(crate) fn plan(report: &PlanReport, name: &str, shape: Shape) -> CallToolResult {
    build(Report::Plan(report, name), false, &[], shape)
}

/// A failed render: `isError: true` with the diagnostics.
pub(crate) fn render_failure(failure: &RenderFailure, name: &str, shape: Shape) -> CallToolResult {
    build(Report::RenderFailure(failure, name), true, &[], shape)
}

/// The providers result.
pub(crate) fn providers(report: &ProvidersReport, shape: Shape) -> CallToolResult {
    build(Report::Providers(report), false, &[], shape)
}

/// The voices result.
pub(crate) fn voices(report: &VoicesReport, shape: Shape) -> CallToolResult {
    build(Report::Voices(report), false, &[], shape)
}

/// A tool-level error with only a message (bad arguments, missing files, engine errors).
pub(crate) fn error_message(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// A tool-level error with a message and structured details (e.g. the valid argument names).
pub(crate) fn error_with_details(message: impl Into<String>, details: Value) -> CallToolResult {
    let mut result = error_message(message);
    result.structured_content = Some(details);
    result
}

/// A tool-level error for an unexpected server-side failure (a panic in the engine, a bug).
pub(crate) fn internal_error(detail: &str) -> CallToolResult {
    error_message(format!(
        "Internal error in the SonicCraft server: {detail}. This is a bug, not a problem with the \
         timeline; please report it."
    ))
}

/// The render result: absolute paths in the report, summary, mirror, and the files as resource links
/// (or, before MCP 2025-06-18, as a text block).
pub(crate) fn render(mut report: RenderReport, base_dir: &Path, shape: Shape) -> CallToolResult {
    absolutize_paths(&mut report, base_dir);
    let files = render_files(&report);
    let extra = if files.is_empty() {
        Vec::new()
    } else if shape.era.resource_links {
        files.iter().map(OutputFile::link).collect()
    } else {
        vec![files_text(&files, report.files.stems.len())]
    };
    build(Report::Render(&report), false, &extra, shape)
}

/// The engine reports absolute paths; make sure of it (relative paths would be ambiguous to a client).
fn absolutize_paths(report: &mut RenderReport, base_dir: &Path) {
    let fix = |path: &mut String| {
        if !Path::new(path.as_str()).is_absolute() {
            tracing::debug!(path = %path, "engine reported a relative path; resolving it against base_dir");
            *path = paths::absolutize(base_dir, Path::new(path.as_str()))
                .display()
                .to_string();
        }
    };
    fix(&mut report.output.path);
    let files = &mut report.files;
    files.subtitles.iter_mut().for_each(|f| fix(&mut f.path));
    files.stems.iter_mut().for_each(|s| fix(&mut s.path));
    if let Some(manifest) = files.manifest.as_mut() {
        fix(manifest);
    }
    if let Some(preview) = files.preview.as_mut() {
        fix(preview);
    }
}

/// MIME type of the main output for a format name.
pub(crate) fn audio_mime(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "aac" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

/// MIME type of a subtitle side file.
fn subtitle_mime(format: &str) -> &'static str {
    match format {
        "vtt" => "text/vtt",
        "srt" => "application/x-subrip",
        _ => "application/json",
    }
}

/// A file written by a render.
struct OutputFile {
    path: String,
    mime: &'static str,
    title: String,
    size: Option<u64>,
}

impl OutputFile {
    fn link(&self) -> ContentBlock {
        let path = Path::new(&self.path);
        let mut resource = Resource::new(paths::file_uri(path), paths::file_name(path))
            .with_title(self.title.clone())
            .with_mime_type(self.mime);
        if let Some(size) = self.size {
            resource = resource.with_size(size);
        }
        ContentBlock::resource_link(resource)
    }
}

fn file_size(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// The audio and each side file that exists, audio first, at most [`MAX_FILE_LINKS`].
fn render_files(report: &RenderReport) -> Vec<OutputFile> {
    let mut files = vec![OutputFile {
        path: report.output.path.clone(),
        mime: audio_mime(&report.output.format),
        title: "Rendered audio".into(),
        size: Some(report.output.bytes),
    }];
    let mut side = |path: &str, mime: &'static str, title: String| {
        if files.len() >= MAX_FILE_LINKS {
            return;
        }
        match file_size(path) {
            Some(size) => files.push(OutputFile {
                path: path.to_string(),
                mime,
                title,
                size: Some(size),
            }),
            None => tracing::debug!(
                path,
                "side file listed in the report does not exist; no link"
            ),
        }
    };
    for sub in &report.files.subtitles {
        side(
            &sub.path,
            subtitle_mime(&sub.format),
            format!("Subtitles ({})", sub.format),
        );
    }
    if let Some(manifest) = &report.files.manifest {
        side(manifest, "application/json", "Render manifest".into());
    }
    if let Some(preview) = &report.files.preview {
        side(preview, "text/html", "Web preview".into());
    }
    for stem in &report.files.stems {
        side(&stem.path, "audio/wav", format!("Stem: {}", stem.track_id));
    }
    files
}

/// The files as text lines, for sessions whose protocol revision has no `resource_link`.
fn files_text(files: &[OutputFile], stems: usize) -> ContentBlock {
    let mut text = String::from("Files written:\n");
    for file in files {
        text.push_str(&format!(
            "- {}: {} ({}{})\n",
            file.title,
            file.path,
            file.mime,
            file.size
                .map(|s| format!(", {s} bytes"))
                .unwrap_or_default()
        ));
    }
    let listed_stems = files
        .iter()
        .filter(|f| f.title.starts_with("Stem: "))
        .count();
    if stems > listed_stems {
        text.push_str(&format!(
            "- … and {} more stems (see files.stems)\n",
            stems - listed_stems
        ));
    }
    ContentBlock::text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soniccraft_engine::{Diagnostic, ItemReport};

    // Reports are built from JSON, as clients see them, so the engine can add fields freely.
    fn from_json<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("a valid report")
    }

    fn shape(detail: Detail) -> Shape {
        Shape {
            detail,
            era: Era {
                resource_links: true,
            },
        }
    }

    fn cue(i: usize) -> ItemReport {
        from_json(json!({
            "id": format!("c{i:04}"),
            "kind": "cue",
            "track_id": "talk",
            "start_ms": i as f64 * 2950.0,
            "end_ms": i as f64 * 2950.0 + 2750.0,
            "duration_ms": 2750.0,
            "duration_source": "estimate",
            "speaker": if i.is_multiple_of(2) { "a" } else { "b" },
            "provider": "mock",
            "voice_id": "a",
            "text": format!("Line number {i} of a long conversation about nothing much."),
        }))
    }

    fn plan_report(cues: usize, diagnostics: Vec<Diagnostic>) -> PlanReport {
        let items: Vec<ItemReport> = (0..cues).map(cue).collect();
        let diagram = items
            .iter()
            .map(|i| {
                format!(
                    "  0:00.00   0:02.75  2.75s  talk  {}  a  estimate  Line number 0 of a long conversation ab...\n",
                    i.id
                )
            })
            .collect::<String>();
        from_json(json!({
            "valid": diagnostics.iter().all(|d| !d.is_error()),
            "diagnostics": diagnostics,
            "duration_ms": cues as f64 * 2950.0,
            "estimated": true,
            "sample_rate": 8000,
            "tracks": [],
            "items": items,
            "ducking": [],
            "synthesis": {"cues": cues, "cached": 0, "characters_to_synthesize": cues * 57, "by_provider": {}},
            "diagram": format!(
                "SonicCraft plan\n\ntime  0s\ntalk  ####\nlegend: # speech\n\n{cues} cues:\n{diagram}"
            ),
        }))
    }

    fn text(result: &CallToolResult, i: usize) -> &str {
        &result.content[i].as_text().unwrap().text
    }

    #[test]
    fn small_reports_are_returned_whole_with_a_mirror() {
        let report = plan_report(3, vec![]);
        let result = plan(&report, "t.yaml", shape(Detail::Auto));
        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured, serde_json::to_value(&report).unwrap());
        let mirror: Value = serde_json::from_str(text(&result, 1)).unwrap();
        assert_eq!(mirror, structured);
        assert!(
            text(&result, 0).contains("3 cues:"),
            "the summary shows the whole diagram"
        );
    }

    #[test]
    fn large_plans_are_compact_and_within_the_agent_budget() {
        for cues in [80, 400, 5_000] {
            let report = plan_report(cues, vec![]);
            let result = plan(&report, "big.yaml", shape(Detail::Auto));
            assert!(
                result_len(&result) <= AGENT_RESULT_BUDGET,
                "{cues}: {}",
                result_len(&result)
            );
            let structured = result.structured_content.as_ref().unwrap();
            assert_eq!(structured["detail"], "summary");
            assert_eq!(structured["items_total"], cues);
            assert_eq!(structured["items_truncated"], true);
            assert_eq!(structured["items"].as_array().unwrap().len(), 20);
            assert!(
                structured["full_report"]
                    .as_str()
                    .unwrap()
                    .contains("soniccraft plan --json")
            );
            let summary = text(&result, 0);
            assert!(
                summary.contains(&format!("Items ({cues} in total")),
                "{summary}"
            );
            assert!(summary.contains("pass detail: \"full\""), "{summary}");
            assert!(
                !summary.contains(" cues:\n"),
                "the diagram tables are left out"
            );
        }
    }

    #[test]
    fn summary_detail_is_always_compact_and_full_keeps_everything() {
        let report = plan_report(3, vec![]);
        let result = plan(&report, "t.yaml", shape(Detail::Summary));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["detail"], "summary");
        assert_eq!(structured["items_truncated"], false);

        let big = plan_report(400, vec![]);
        let result = plan(&big, "t.yaml", shape(Detail::Full));
        assert_eq!(
            result.structured_content.unwrap(),
            serde_json::to_value(&big).unwrap()
        );
    }

    #[test]
    fn oversized_full_results_shrink_instead_of_failing() {
        // About 30,000 cues with long texts: far above the message limit.
        let mut report = plan_report(30_000, vec![]);
        for item in &mut report.items {
            item.text = Some("y".repeat(300));
        }
        let diagnostics: Vec<Diagnostic> = (0..30_000)
            .map(|i| {
                Diagnostic::error("unknown_field", format!("unknown field `x{i}`"))
                    .with_path(format!("tracks[0].sequence[{i}].x"))
                    .with_hint("Remove it")
            })
            .collect();
        report.diagnostics = diagnostics;
        report.valid = false;
        let result = plan(&report, "huge.yaml", shape(Detail::Full));
        let len = result_len(&result);
        assert!(len <= MESSAGE_RESULT_BUDGET, "{len}");
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["detail"], "full");
        assert_eq!(structured["items_total"], 30_000);
        assert_eq!(structured["diagnostics_total"], 30_000);
        assert!(
            structured["full_report"]
                .as_str()
                .unwrap()
                .contains("larger than one MCP message"),
            "{}",
            structured["full_report"]
        );
        assert!(text(&result, 1).contains("not repeated"));

        // The same report in auto mode stays within the agent budget.
        let auto = plan(&report, "huge.yaml", shape(Detail::Auto));
        assert!(
            result_len(&auto) <= AGENT_RESULT_BUDGET,
            "{}",
            result_len(&auto)
        );
        let structured = auto.structured_content.as_ref().unwrap();
        assert_eq!(structured["diagnostic_counts"]["errors"], 30_000);
        assert!(!structured["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn many_long_diagnostics_still_fit() {
        let diagnostics: Vec<Diagnostic> = (0..200)
            .map(|i| {
                Diagnostic::warning(
                    "speech_overlap",
                    format!("`c{i:04}` overlaps {}", "z".repeat(500)),
                )
                .with_hint("h".repeat(500))
                .with_path(format!("tracks[0].sequence[{i}]"))
            })
            .collect();
        let report: ValidationReport =
            from_json(json!({"valid": true, "diagnostics": diagnostics}));
        let result = validation(&report, "t.yaml", shape(Detail::Auto));
        assert!(
            result_len(&result) <= AGENT_RESULT_BUDGET,
            "{}",
            result_len(&result)
        );
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["diagnostics_total"], 200);
        assert!(!structured["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn render_files_follow_the_protocol_era() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("out.wav");
        std::fs::write(&audio, b"RIFF").unwrap();
        let vtt = dir.path().join("out.vtt");
        std::fs::write(&vtt, b"WEBVTT\n").unwrap();
        let report: RenderReport = from_json(json!({
            "output": {
                "path": audio.display().to_string(),
                "format": "wav",
                "sample_rate": 48000,
                "channels": 2,
                "bit_depth": 24,
                "duration_ms": 1000.0,
                "bytes": 4,
            },
            "loudness": {
                "integrated_lufs": -16.0,
                "true_peak_dbtp": -1.5,
                "sample_peak_dbfs": -2.0,
                "loudness_range_lu": null,
                "target_lufs": -16.0,
                "true_peak_target_dbtp": -1.0,
                "normalized": true,
                "gain_db": 1.0,
                "limiter_max_reduction_db": 0.0,
                "iterations": 1,
            },
            "files": {"subtitles": [{"format": "vtt", "path": vtt.display().to_string()}]},
            "tracks": [],
            "items": [],
            "ducking": [],
            "diagnostics": [],
        }));
        let modern = render(report.clone(), dir.path(), shape(Detail::Auto));
        let types: Vec<String> = modern
            .content
            .iter()
            .map(|c| {
                serde_json::to_value(c).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(types, ["text", "text", "resource_link", "resource_link"]);

        let old = render(
            report,
            dir.path(),
            Shape {
                detail: Detail::Auto,
                era: Era::of(Some(&ProtocolVersion::V_2025_03_26)),
            },
        );
        let types: Vec<String> = old
            .content
            .iter()
            .map(|c| {
                serde_json::to_value(c).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(types, ["text", "text", "text"]);
        let files = text(&old, 2);
        assert!(files.starts_with("Files written:\n"), "{files}");
        assert!(files.contains(&audio.display().to_string()), "{files}");
        assert!(
            files.contains("Subtitles (vtt)") && files.contains("text/vtt"),
            "{files}"
        );
    }

    #[test]
    fn eras() {
        assert!(!Era::of(None).resource_links);
        assert!(!Era::of(Some(&ProtocolVersion::V_2024_11_05)).resource_links);
        assert!(!Era::of(Some(&ProtocolVersion::V_2025_03_26)).resource_links);
        assert!(Era::of(Some(&ProtocolVersion::V_2025_06_18)).resource_links);
        assert!(Era::of(Some(&ProtocolVersion::V_2025_11_25)).resource_links);
        assert!(Era::of(Some(&ProtocolVersion::V_2026_07_28)).resource_links);
    }

    #[test]
    fn mime_types() {
        assert_eq!(audio_mime("mp3"), "audio/mpeg");
        assert_eq!(audio_mime("WAV"), "audio/wav");
        assert_eq!(audio_mime("opus"), "audio/ogg");
        assert_eq!(audio_mime("m4a"), "audio/mp4");
        assert_eq!(subtitle_mime("vtt"), "text/vtt");
        assert_eq!(subtitle_mime("transcript"), "application/json");
    }
}
