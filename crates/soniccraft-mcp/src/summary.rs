//! Compact, human- and LLM-readable summaries of engine reports: the first text block of every tool
//! result. The report itself is the result's `structuredContent` (and the second text block).
//!
//! Diagnostics are rendered one per line as `error[code] path (line:column): message — hint`, errors
//! first, so an agent can apply the hints mechanically. Summaries stay short whatever the size of the
//! timeline: every list is capped ([`SummaryCaps`]), the plan shows its item table only for the first and
//! last items, and a long plan diagram is shown without its tables (or not at all).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use soniccraft_engine::{
    Diagnostic, ItemKind, ItemReport, PlanReport, ProvidersReport, RenderFailure, RenderReport,
    Severity, ValidationReport, VoicesReport,
};

use crate::compact;

/// Most voices listed in a summary.
const MAX_VOICE_LINES: usize = 100;

/// Longest diagnostic line; longer lines end in `…` (the report has the whole text).
const MAX_DIAGNOSTIC_LINE_CHARS: usize = 600;

/// Longest cue text quoted in the plan's item table.
const ITEM_TEXT_CHARS: usize = 48;

/// Where the parts a summary leaves out can be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rest {
    /// `structuredContent` has the whole report.
    InStructured,
    /// `structuredContent` is a compact report: `detail: "full"` returns everything.
    AskFull,
    /// A full report shortened to fit one message: its `full_report` says where everything is.
    TooLarge,
}

/// The parts of a report a summary can leave out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Diagnostics,
    Items,
    Diagram,
}

/// How much a summary lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SummaryCaps {
    /// Most diagnostic lines.
    pub(crate) diagnostics: usize,
    /// Rows of the plan's item table at each end (shown when the whole diagram is not).
    pub(crate) table_edge: usize,
    /// Longest plan diagram shown, in characters (0 shows none).
    pub(crate) diagram: usize,
    /// Most entries of inline lists: moved cues, ducked tracks, stems, providers.
    pub(crate) list: usize,
    /// Where the parts left out are.
    pub(crate) rest: Rest,
}

impl SummaryCaps {
    /// The usual summary.
    pub(crate) const STANDARD: SummaryCaps = SummaryCaps {
        diagnostics: 40,
        table_edge: 15,
        diagram: 6_000,
        list: 10,
        rest: Rest::InStructured,
    };

    /// A shorter summary, when the usual one does not fit the result budget.
    pub(crate) const SHORT: SummaryCaps = SummaryCaps {
        diagnostics: 15,
        table_edge: 8,
        diagram: 3_000,
        list: 5,
        rest: Rest::InStructured,
    };

    /// A summary that still lists a little of everything.
    pub(crate) const TINY: SummaryCaps = SummaryCaps {
        diagnostics: 5,
        table_edge: 3,
        diagram: 0,
        list: 3,
        rest: Rest::InStructured,
    };

    /// The shortest summary: headline facts and the first diagnostics.
    pub(crate) const MINIMAL: SummaryCaps = SummaryCaps {
        diagnostics: 3,
        table_edge: 0,
        diagram: 0,
        list: 3,
        rest: Rest::InStructured,
    };

    /// The same caps, pointing to `rest` for what they leave out.
    pub(crate) const fn with_rest(self, rest: Rest) -> SummaryCaps {
        SummaryCaps { rest, ..self }
    }

    /// Where the left-out `part` can be found.
    fn rest(&self, part: Part) -> &'static str {
        match (self.rest, part) {
            (Rest::InStructured, Part::Diagnostics) => "all of them are in structuredContent",
            (Rest::InStructured, Part::Items) => "every item is in structuredContent.items",
            (Rest::InStructured, Part::Diagram) => "the whole diagram is structuredContent.diagram",
            (Rest::AskFull, Part::Diagnostics) => "pass detail: \"full\" to get all of them",
            (Rest::AskFull, Part::Items) => "pass detail: \"full\" to get every item",
            (Rest::AskFull, Part::Diagram) => "pass detail: \"full\" to get the whole diagram",
            (Rest::TooLarge, _) => "the whole report does not fit in one message; see full_report",
        }
    }
}

/// One diagnostic as a single line.
pub(crate) fn diagnostic_line(d: &Diagnostic) -> String {
    let mut line = format!("{}[{}]", d.severity, d.code);
    if let Some(path) = d.path.as_deref().filter(|p| !p.is_empty()) {
        line.push(' ');
        line.push_str(path);
    }
    match (d.line, d.column) {
        (Some(l), Some(c)) => {
            let _ = write!(line, " ({l}:{c})");
        }
        (Some(l), None) => {
            let _ = write!(line, " (line {l})");
        }
        _ => {}
    }
    line.push_str(": ");
    line.push_str(&one_line(&d.message));
    if let Some(hint) = d.hint.as_deref().filter(|h| !h.trim().is_empty()) {
        line.push_str(" — ");
        line.push_str(&one_line(hint));
    }
    if line.chars().count() > MAX_DIAGNOSTIC_LINE_CHARS {
        line = compact::shorten(&line, MAX_DIAGNOSTIC_LINE_CHARS);
    }
    line
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `2 errors, 1 warning, 3 infos` (zero groups omitted; `no diagnostics` when empty).
pub(crate) fn counts_phrase(diagnostics: &[Diagnostic]) -> String {
    let (errors, warnings, infos) = soniccraft_core::diagnostics::counts(diagnostics);
    let parts: Vec<String> = [(errors, "error"), (warnings, "warning"), (infos, "info")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| plural(n, word))
        .collect();
    if parts.is_empty() {
        "no diagnostics".into()
    } else {
        parts.join(", ")
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The diagnostics block: errors, then warnings, then infos (each group in report order), capped; the
/// line after the cap counts the rest by code.
fn push_diagnostics(out: &mut String, diagnostics: &[Diagnostic], caps: &SummaryCaps) {
    if diagnostics.is_empty() {
        return;
    }
    let _ = writeln!(out, "Diagnostics ({}):", counts_phrase(diagnostics));
    let ordered = [Severity::Error, Severity::Warning, Severity::Info]
        .into_iter()
        .flat_map(|severity| diagnostics.iter().filter(move |d| d.severity == severity));
    let mut rest: BTreeMap<(Severity, &str), usize> = BTreeMap::new();
    for (shown, d) in ordered.enumerate() {
        if shown < caps.diagnostics {
            let _ = writeln!(out, "{}", diagnostic_line(d));
        } else {
            *rest.entry((d.severity, d.code.as_str())).or_default() += 1;
        }
    }
    if !rest.is_empty() {
        let left = diagnostics.len().saturating_sub(caps.diagnostics);
        // Errors first, then the most frequent codes.
        let mut groups: Vec<((Severity, &str), usize)> = rest.into_iter().collect();
        groups.sort_by(|a, b| b.0.0.cmp(&a.0.0).then(b.1.cmp(&a.1)));
        let mut parts: Vec<String> = groups
            .iter()
            .take(6)
            .map(|((severity, code), n)| format!("{severity}[{code}] ×{n}"))
            .collect();
        if groups.len() > 6 {
            parts.push("…".into());
        }
        let _ = writeln!(
            out,
            "… and {left} more ({}; {}).",
            parts.join(", "),
            caps.rest(Part::Diagnostics)
        );
    }
}

/// `items` joined with `, `, at most `max` of them, then `and N more`.
fn capped_list(items: &[String], max: usize, separator: &str) -> String {
    if items.len() <= max {
        return items.join(separator);
    }
    format!(
        "{}{separator}and {} more",
        items[..max].join(separator),
        items.len() - max
    )
}

/// `20.62 s`.
pub(crate) fn seconds(ms: f64) -> String {
    format!("{:.2} s", ms / 1000.0)
}

/// `822.4 KiB`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return plural(bytes as usize, "byte");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

/// Summary of a [`ValidationReport`]; `name` is the timeline file or `inline yaml_content`.
pub(crate) fn validation(report: &ValidationReport, name: &str, caps: &SummaryCaps) -> String {
    let mut out = String::new();
    if report.valid {
        let notes = counts_phrase(&report.diagnostics);
        let _ = writeln!(out, "Timeline {name} is VALID ({notes}).");
    } else {
        let _ = writeln!(
            out,
            "Timeline {name} is INVALID: {}.",
            counts_phrase(&report.diagnostics)
        );
    }
    if let Some(s) = &report.summary {
        let _ = writeln!(
            out,
            "Contents: {} tracks, {} speech cues, {} clips; speakers: {}; providers: {}; {} characters of \
             cue text, {} cues already cached; output {} at {} Hz, target {:.1} LUFS.",
            s.tracks,
            s.speech_cues,
            s.clips,
            capped_list_or_none(&s.speakers, caps.list),
            capped_list_or_none(&s.providers, caps.list),
            s.characters,
            s.cached_cues,
            s.output_format,
            s.sample_rate,
            s.target_lufs
        );
    }
    push_diagnostics(&mut out, &report.diagnostics, caps);
    if report.valid {
        out.push_str(
            "Next step: call soniccraft_dry_run to check timing and cost, then soniccraft_render.\n",
        );
    } else {
        out.push_str(
            "Next step: fix every error (apply each hint at its path), then call \
             soniccraft_validate_timeline again.\n",
        );
    }
    out
}

fn capped_list_or_none(items: &[String], max: usize) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        capped_list(items, max, ", ")
    }
}

/// Summary of a [`PlanReport`]: totals, cost, moved cues, ducking, the diagram (whole when it is short,
/// else without its cue and clip tables, followed by the first and last items) and the diagnostics.
pub(crate) fn plan(report: &PlanReport, name: &str, caps: &SummaryCaps) -> String {
    let mut out = String::new();
    if report.valid {
        let _ = writeln!(
            out,
            "Dry run of {name}: VALID. Total length {} ({}), {} Hz, {} items on {} tracks.",
            seconds(report.duration_ms),
            if report.estimated {
                "estimated speech durations"
            } else {
                "exact speech durations: cached or measured"
            },
            report.sample_rate,
            report.items.len(),
            report.tracks.len()
        );
    } else {
        let _ = writeln!(
            out,
            "Dry run of {name}: INVALID ({}). Fix the errors below and validate again.",
            counts_phrase(&report.diagnostics)
        );
    }
    let synthesis = &report.synthesis;
    if synthesis.cues > 0 {
        let providers: Vec<String> = synthesis
            .by_provider
            .iter()
            .map(|(name, p)| {
                format!(
                    "{name}: {} ({} cached), {} characters",
                    plural(p.cues, "cue"),
                    p.cached,
                    p.characters
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "Synthesis: {}, {} cached, {} characters to synthesize{}.",
            plural(synthesis.cues, "cue"),
            synthesis.cached,
            synthesis.characters_to_synthesize,
            if providers.is_empty() {
                String::new()
            } else {
                format!(" ({})", capped_list(&providers, caps.list, "; "))
            }
        );
    }
    let pushed: Vec<String> = report
        .items
        .iter()
        .filter_map(|item| {
            item.pushed_from_ms.map(|from| {
                format!(
                    "{} from {} to {}",
                    item.id,
                    seconds(from),
                    seconds(item.start_ms)
                )
            })
        })
        .collect();
    if !pushed.is_empty() {
        let _ = writeln!(
            out,
            "Moved by the overlap policy: {}.",
            capped_list(&pushed, caps.list, "; ")
        );
    }
    for duck in report.ducking.iter().take(caps.list) {
        // Dry runs derive the regions from the planned item intervals, whatever the configured mode.
        let _ = writeln!(
            out,
            "Ducking: {} -{:.1} dB under {} in {} (estimated from the planned cue timing).",
            duck.track_id,
            duck.reduction_db.abs(),
            list_or_none(&duck.triggers),
            plural(duck.regions.len(), "region"),
        );
    }
    if report.ducking.len() > caps.list {
        let _ = writeln!(
            out,
            "… and {} more ducked tracks.",
            report.ducking.len() - caps.list
        );
    }
    let (diagram, cut) = compact::fit_diagram(&report.diagram, caps.diagram);
    if !diagram.trim().is_empty() {
        out.push('\n');
        out.push_str(diagram.trim_end());
        out.push_str("\n\n");
    }
    if cut {
        let total = report.diagram.trim_end().chars().count();
        let whole = caps.rest(Part::Diagram);
        if diagram.is_empty() {
            let _ = writeln!(
                out,
                "The timeline diagram ({total} characters) is left out here; {whole}."
            );
        } else {
            let _ = writeln!(
                out,
                "The diagram's cue and clip tables are left out here ({total} characters in all; {whole})."
            );
        }
        push_item_table(&mut out, &report.items, caps);
    }
    push_diagnostics(&mut out, &report.diagnostics, caps);
    if report.valid {
        out.push_str(
            "Next step: if the order, timing and cost look right, call soniccraft_render with the same \
             timeline.\n",
        );
    }
    out
}

/// The first and last `caps.table_edge` items, one per line.
fn push_item_table(out: &mut String, items: &[ItemReport], caps: &SummaryCaps) {
    if items.is_empty() || caps.table_edge == 0 {
        return;
    }
    let edge = caps.table_edge;
    if items.len() <= 2 * edge {
        let _ = writeln!(out, "Items ({}, by start time):", items.len());
        for item in items {
            let _ = writeln!(out, "{}", item_line(item));
        }
        return;
    }
    let _ = writeln!(
        out,
        "Items ({} in total, by start time; the first {edge} and the last {edge}):",
        items.len()
    );
    for item in &items[..edge] {
        let _ = writeln!(out, "{}", item_line(item));
    }
    let _ = writeln!(
        out,
        "- … {} more items ({}) …",
        items.len() - 2 * edge,
        caps.rest(Part::Items)
    );
    for item in &items[items.len() - edge..] {
        let _ = writeln!(out, "{}", item_line(item));
    }
}

/// `- 12.40–15.20 s cue c012 (track talk, speaker a, estimate, moved from 11.90 s): "Line number 12…"`.
fn item_line(item: &ItemReport) -> String {
    let kind = match item.kind {
        ItemKind::Cue => "cue",
        ItemKind::Pause => "pause",
        ItemKind::Clip => "clip",
    };
    let mut facts = vec![format!("track {}", item.track_id)];
    if let Some(speaker) = &item.speaker {
        facts.push(format!("speaker {speaker}"));
    }
    if item.kind != ItemKind::Pause {
        let source = serde_json::to_value(item.duration_source)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        facts.push(source);
    }
    if let Some(from) = item.pushed_from_ms {
        facts.push(format!("moved from {}", seconds(from)));
    }
    if let Some(source) = &item.source {
        facts.push(compact::shorten(&one_line(source), ITEM_TEXT_CHARS));
    }
    if item.open_ended {
        facts.push("open-ended".into());
    }
    if item.looped {
        facts.push("loop".into());
    }
    let mut line = format!(
        "- {:.2}–{:.2} s {kind} {} ({})",
        item.start_ms / 1000.0,
        item.end_ms / 1000.0,
        item.id,
        facts.join(", ")
    );
    if let Some(text) = item.text.as_deref().filter(|t| !t.trim().is_empty()) {
        let _ = write!(
            line,
            ": \"{}\"",
            compact::shorten(&one_line(text), ITEM_TEXT_CHARS)
        );
    }
    line
}

/// Summary of a successful [`RenderReport`].
pub(crate) fn render(report: &RenderReport, caps: &SummaryCaps) -> String {
    let mut out = String::new();
    let o = &report.output;
    let _ = writeln!(out, "Rendered {}", o.path);
    let quality = match (o.bit_depth, o.bitrate_bps) {
        (_, Some(bps)) => format!(", {} kbps", (bps + 500) / 1000),
        (Some(bits), None) => format!(", {bits}-bit"),
        (None, None) => String::new(),
    };
    let channels = match o.channels {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n} channels"),
    };
    let _ = writeln!(
        out,
        "Audio: {}, {} Hz, {channels}{quality}, {}, {}.",
        o.format,
        o.sample_rate,
        seconds(o.duration_ms),
        human_bytes(o.bytes)
    );
    let l = &report.loudness;
    let integrated = match l.integrated_lufs {
        Some(lufs) => format!(
            "{lufs:.1} LUFS integrated (target {:.1} LUFS)",
            l.target_lufs
        ),
        None => format!(
            "integrated loudness not measurable: silent or too short (target {:.1} LUFS)",
            l.target_lufs
        ),
    };
    let peak = match l.true_peak_dbtp {
        Some(tp) => format!(
            "true peak {tp:.1} dBTP (ceiling {:.1} dBTP)",
            l.true_peak_target_dbtp
        ),
        None => format!(
            "no true peak: silent (ceiling {:.1} dBTP)",
            l.true_peak_target_dbtp
        ),
    };
    let range = l
        .loudness_range_lu
        .map(|lra| format!(", loudness range {lra:.1} LU"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "Loudness: {integrated}, {peak}{range}; {} gain {:+.1} dB, limiter reduction up to {:.1} dB, {}.",
        if l.normalized {
            "normalization"
        } else {
            "no normalization,"
        },
        l.gain_db,
        l.limiter_max_reduction_db.abs(),
        plural(l.iterations as usize, "pass")
    );
    let files = &report.files;
    if !files.subtitles.is_empty() {
        let subs: Vec<String> = files
            .subtitles
            .iter()
            .map(|f| format!("{} {}", f.format, f.path))
            .collect();
        let _ = writeln!(out, "Subtitles: {}.", subs.join("; "));
    }
    if !files.stems.is_empty() {
        let stems: Vec<String> = files
            .stems
            .iter()
            .map(|s| format!("{} {}", s.track_id, s.path))
            .collect();
        let _ = writeln!(out, "Stems: {}.", capped_list(&stems, caps.list, "; "));
    }
    if let Some(manifest) = &files.manifest {
        let _ = writeln!(out, "Manifest: {manifest}.");
    }
    if let Some(preview) = &files.preview {
        let _ = writeln!(out, "Preview: {preview}.");
    }
    let s = &report.synthesis;
    let _ = writeln!(
        out,
        "Synthesis: {}; {} from cache, {} synthesized (providers called: {}).",
        plural(s.cues, "cue"),
        s.cache_hits,
        s.synthesized,
        capped_list_or_none(&s.providers_called, caps.list)
    );
    if let Some(note) = extension_note(o) {
        let _ = writeln!(out, "{note}");
    }
    push_diagnostics(&mut out, &report.diagnostics, caps);
    out
}

/// A note when the file's extension names another format than its contents (the timeline's
/// `output.format` wins over the extension of `output_path`).
fn extension_note(output: &soniccraft_engine::OutputReport) -> Option<String> {
    let ext = Path::new(&output.path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    let named = soniccraft_core::ir::OutputFormat::parse(&ext)?;
    let actual = soniccraft_core::ir::OutputFormat::parse(&output.format)?;
    (named != actual).then(|| {
        format!(
            "Note: the file holds {actual} data although its extension is .{ext}. To write {named}, call \
             soniccraft_render again with `format: \"{named}\"`; to keep {actual}, use an output_path ending \
             in .{actual}."
        )
    })
}

/// Summary of a failed render.
pub(crate) fn render_failure(failure: &RenderFailure, name: &str, caps: &SummaryCaps) -> String {
    let mut out = String::new();
    if failure.cancelled {
        let _ = writeln!(
            out,
            "Render of {name} CANCELLED: {}. Partial output files were removed.",
            one_line(&failure.message)
        );
    } else {
        let _ = writeln!(
            out,
            "Render of {name} FAILED: {}",
            one_line(&failure.message)
        );
    }
    push_diagnostics(&mut out, &failure.diagnostics, caps);
    if !failure.cancelled {
        out.push_str(
            "Next step: fix the problems above (check with soniccraft_validate_timeline), then call \
             soniccraft_render again.\n",
        );
    }
    out
}

/// Summary of a [`ProvidersReport`].
pub(crate) fn providers(report: &ProvidersReport) -> String {
    let mut out = String::new();
    let usable: Vec<String> = report
        .providers
        .iter()
        .filter(|p| p.available)
        .map(|p| p.name.clone())
        .collect();
    let _ = writeln!(
        out,
        "{} built-in TTS providers; usable now: {}.",
        report.providers.len(),
        list_or_none(&usable)
    );
    for p in &report.providers {
        let mut facts = Vec::new();
        if let Some(voice) = &p.default_voice {
            facts.push(format!("default voice {voice}"));
        }
        if let Some(model) = &p.default_model {
            facts.push(format!("default model {model}"));
        }
        if let Some(env) = &p.api_key_env {
            facts.push(format!("API key env {env}"));
        }
        facts.push(if p.local { "local" } else { "cloud" }.to_string());
        if p.word_timestamps {
            facts.push("word timestamps".into());
        }
        if !p.aliases.is_empty() {
            facts.push(format!("aliases {}", p.aliases.join(", ")));
        }
        let _ = writeln!(
            out,
            "- {} ({}): {} — {}. {}.",
            p.name,
            p.display_name,
            if p.available {
                "available"
            } else {
                "NOT available"
            },
            one_line(&p.status).trim_end_matches('.'),
            facts.join("; ")
        );
    }
    out.push_str(
        "Use a provider's name (or alias) as `provider:` in a timeline. A missing API key must be exported \
         by the user as the named environment variable; never put keys in the timeline.\n",
    );
    out
}

/// Summary of a [`VoicesReport`].
pub(crate) fn voices(report: &VoicesReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}: {} (use `id` as voice_id):",
        report.provider,
        plural(report.voices.len(), "voice")
    );
    for voice in report.voices.iter().take(MAX_VOICE_LINES) {
        let mut facts: Vec<&str> = Vec::new();
        if let Some(language) = voice.language.as_deref() {
            facts.push(language);
        }
        if let Some(gender) = voice.gender.as_deref() {
            facts.push(gender);
        }
        let mut line = format!("- {}", voice.id);
        if voice.name != voice.id {
            let _ = write!(line, " — {}", voice.name);
        }
        if !facts.is_empty() {
            let _ = write!(line, " ({})", facts.join(", "));
        }
        if let Some(description) = voice.description.as_deref().filter(|d| !d.is_empty()) {
            let _ = write!(line, ": {}", one_line(description));
        }
        let _ = writeln!(out, "{line}");
    }
    if report.voices.len() > MAX_VOICE_LINES {
        let _ = writeln!(
            out,
            "… showing {MAX_VOICE_LINES} of {}; all voices are in the JSON report.",
            report.voices.len()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // Reports are built from JSON, as clients see them, so the engine can add fields freely.
    fn from_json<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("a valid report")
    }

    #[test]
    fn diagnostic_lines() {
        let d = Diagnostic::error("unknown_field", "unknown field `fade_in`\n  in clip")
            .with_path("tracks[0].fade_in")
            .with_hint("Did you mean `fade_in_ms`?")
            .at(12, 5);
        assert_eq!(
            diagnostic_line(&d),
            "error[unknown_field] tracks[0].fade_in (12:5): unknown field `fade_in` in clip — Did you mean \
             `fade_in_ms`?"
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        );
        let d = Diagnostic::info("cue_pushed", "moved");
        assert_eq!(diagnostic_line(&d), "info[cue_pushed]: moved");
    }

    #[test]
    fn counts_and_units() {
        let diags = vec![
            Diagnostic::warning("a", "x"),
            Diagnostic::error("b", "y"),
            Diagnostic::error("c", "z"),
        ];
        assert_eq!(counts_phrase(&diags), "2 errors, 1 warning");
        assert_eq!(counts_phrase(&[]), "no diagnostics");
        assert_eq!(seconds(20616.94), "20.62 s");
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(842_112), "822.4 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn errors_come_first_and_are_capped() {
        let mut diags: Vec<Diagnostic> = (0..70)
            .map(|i| Diagnostic::info("cue_pushed", format!("info {i}")))
            .collect();
        diags.push(Diagnostic::error("unknown_reference", "bad ref").with_path("tracks[1]"));
        diags.extend((0..5).map(|i| Diagnostic::warning("speech_overlap", format!("w {i}"))));
        let report: ValidationReport = from_json(json!({"valid": false, "diagnostics": diags}));
        let text = validation(&report, "/abs/t.yaml", &SummaryCaps::STANDARD);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Timeline /abs/t.yaml is INVALID: 1 error, 5 warnings, 70 infos."
        );
        assert!(lines[2].starts_with("error[unknown_reference] tracks[1]: bad ref"));
        assert!(lines[3].starts_with("warning[speech_overlap]"), "{text}");
        assert!(
            text.contains(
                "… and 36 more (info[cue_pushed] ×36; all of them are in structuredContent)."
            ),
            "{text}"
        );
        assert!(text.contains("soniccraft_validate_timeline again"));

        let compact = validation(
            &report,
            "/abs/t.yaml",
            &SummaryCaps::TINY.with_rest(Rest::AskFull),
        );
        assert!(
            compact.contains("… and 71 more (warning[speech_overlap] ×1, info[cue_pushed] ×70; pass detail: \"full\""),
            "{compact}"
        );
    }

    #[test]
    fn long_diagnostic_lines_are_cut() {
        let d = Diagnostic::error("x", "m".repeat(5_000));
        let line = diagnostic_line(&d);
        assert_eq!(line.chars().count(), MAX_DIAGNOSTIC_LINE_CHARS);
        assert!(line.ends_with('…'));
    }

    fn item(i: usize) -> Value {
        let mut item = json!({
            "id": format!("c{i:03}"),
            "kind": "cue",
            "track_id": "talk",
            "start_ms": i as f64 * 1000.0,
            "end_ms": i as f64 * 1000.0 + 750.0,
            "duration_ms": 750.0,
            "duration_source": "estimate",
            "speaker": "a",
            "provider": "mock",
            "text": format!("Line number {i} of a long conversation about nothing much at all."),
        });
        if i == 7 {
            item["pushed_from_ms"] = json!(6500.0);
        }
        item
    }

    fn plan_report(n: usize, diagram: String) -> PlanReport {
        let items: Vec<Value> = (0..n).map(item).collect();
        from_json(json!({
            "valid": true,
            "diagnostics": [],
            "duration_ms": n as f64 * 1000.0,
            "estimated": true,
            "sample_rate": 48000,
            "tracks": [],
            "items": items,
            "ducking": [],
            "synthesis": {"cues": n, "cached": 0, "characters_to_synthesize": 10, "by_provider": {}},
            "diagram": diagram,
        }))
    }

    #[test]
    fn short_plans_show_the_whole_diagram() {
        let diagram = "SonicCraft plan\n\ntalk  ###\n\n3 cues:\n  row\n".to_string();
        let text = plan(
            &plan_report(3, diagram.clone()),
            "t.yaml",
            &SummaryCaps::STANDARD,
        );
        assert!(text.contains(diagram.trim_end()), "{text}");
        assert!(!text.contains("Items ("), "no extra table: {text}");
    }

    #[test]
    fn long_plans_show_the_lanes_and_the_ends_of_the_item_list() {
        let rows: String = (0..400)
            .map(|i| format!("  row {i:03} {}\n", "-".repeat(80)))
            .collect();
        let diagram =
            format!("SonicCraft plan: big\n\ntalk  ######\nlegend: # speech\n\n400 cues:\n{rows}");
        let report = plan_report(400, diagram);
        let text = plan(
            &report,
            "big.yaml",
            &SummaryCaps::STANDARD.with_rest(Rest::AskFull),
        );
        assert!(text.contains("talk  ######\nlegend: # speech"), "{text}");
        assert!(!text.contains("row 000"), "the diagram's table is left out");
        assert!(
            text.contains("cue and clip tables are left out here"),
            "{text}"
        );
        assert!(
            text.contains("Items (400 in total, by start time; the first 15 and the last 15):")
        );
        assert!(
            text.contains(
                "- 0.00–0.75 s cue c000 (track talk, speaker a, estimate): \"Line number 0"
            )
        );
        assert!(
            text.contains("c007 (track talk, speaker a, estimate, moved from 6.50 s)"),
            "{text}"
        );
        assert!(text.contains("- … 370 more items (pass detail: \"full\" to get every item) …"));
        assert!(text.contains("c399"));
        assert!(!text.contains("c200"));
        assert!(text.contains("Moved by the overlap policy: c007 from 6.50 s to 7.00 s."));
        assert!(text.len() < 12_000, "{} chars", text.len());
    }

    #[test]
    fn render_summary_handles_silence() {
        let report: RenderReport = from_json(json!({
            "output": {
                "path": "/abs/out.wav",
                "format": "wav",
                "sample_rate": 48000,
                "channels": 2,
                "bit_depth": 24,
                "duration_ms": 1500.0,
                "bytes": 432_044,
            },
            "loudness": {
                "integrated_lufs": null,
                "true_peak_dbtp": null,
                "sample_peak_dbfs": null,
                "loudness_range_lu": null,
                "target_lufs": -16.0,
                "true_peak_target_dbtp": -1.0,
                "normalized": true,
                "gain_db": 0.0,
                "limiter_max_reduction_db": 0.0,
                "iterations": 0,
            },
            "files": {},
            "tracks": [],
            "items": [],
            "ducking": [],
            "diagnostics": [Diagnostic::warning("silent_output", "the master is silent")],
        }));
        let text = render(&report, &SummaryCaps::STANDARD);
        assert!(text.starts_with("Rendered /abs/out.wav\n"), "{text}");
        assert!(
            text.contains("wav, 48000 Hz, stereo, 24-bit, 1.50 s, 421.9 KiB"),
            "{text}"
        );
        assert!(text.contains("not measurable"), "{text}");
        assert!(text.contains("warning[silent_output]"), "{text}");
        assert!(!text.contains("Note: the file holds"), "{text}");

        let mut mp3 = report.clone();
        mp3.output.path = "/abs/out.mp3".into();
        let text = render(&mp3, &SummaryCaps::STANDARD);
        assert!(
            text.contains(
                "Note: the file holds wav data although its extension is .mp3. To write mp3, call \
                 soniccraft_render again with `format: \"mp3\"`"
            ),
            "{text}"
        );
    }
}
