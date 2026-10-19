//! Structured diagnostics shared by every SonicCraft crate, the CLI and the MCP server.
//!
//! Diagnostic codes are a stable public API (agents key their repairs on them); add new codes to
//! [`codes`] and to `docs/DESIGN.md` §7, never rename existing ones.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How serious a diagnostic is. Ordered `Info < Warning < Error`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A note; nothing needs to change.
    Info,
    /// Probably unintended; rendering still works.
    Warning,
    /// The timeline cannot be rendered until this is fixed.
    Error,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
}

/// One problem or note about a timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    /// How serious the problem is.
    pub severity: Severity,
    /// Stable snake_case code, see [`codes`].
    pub code: String,
    /// Human-readable description of the problem.
    pub message: String,
    /// Location in the document, e.g. `tracks[2].sequence[0].start_relative.after_cue`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 1-based line in the source document (YAML/JSON), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column in the source document, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// A concrete, mechanical fix suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Ids of tracks/cues/clips involved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
}

impl Diagnostic {
    /// A diagnostic without path, position, hint or related ids.
    pub fn new(severity: Severity, code: &str, message: impl Into<String>) -> Self {
        Diagnostic {
            severity,
            code: code.to_string(),
            message: message.into(),
            path: None,
            line: None,
            column: None,
            hint: None,
            related: Vec::new(),
        }
    }

    /// An error diagnostic.
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, message)
    }

    /// A warning diagnostic.
    pub fn warning(code: &str, message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, code, message)
    }

    /// An informational diagnostic.
    pub fn info(code: &str, message: impl Into<String>) -> Self {
        Self::new(Severity::Info, code, message)
    }

    /// Set the document path (`tracks[2].sequence[0].text`).
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the fix suggestion.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Add ids of related tracks, cues or clips. Ids already listed are not added again, so `related` never
    /// holds duplicates (first occurrence order is kept).
    pub fn with_related<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        // A grouped diagnostic can relate thousands of cues: dedupe with a set, not `Vec::contains`.
        let mut seen: HashSet<String> = self.related.iter().cloned().collect();
        for id in ids {
            let id = id.into();
            if seen.insert(id.clone()) {
                self.related.push(id);
            }
        }
        self
    }

    /// Set the 1-based line and column.
    pub fn at(mut self, line: u32, column: u32) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    /// True for errors.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{}]: {}", self.severity, self.code, self.message)?;
        if let Some(path) = &self.path {
            write!(f, " (at {path}")?;
            if let Some(line) = self.line {
                write!(f, ", line {line}")?;
                if let Some(col) = self.column {
                    write!(f, ":{col}")?;
                }
            }
            write!(f, ")")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

/// True if any diagnostic is an error.
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(Diagnostic::is_error)
}

/// Count `(errors, warnings, infos)`.
pub fn counts(diagnostics: &[Diagnostic]) -> (usize, usize, usize) {
    diagnostics
        .iter()
        .fold((0, 0, 0), |(e, w, i), d| match d.severity {
            Severity::Error => (e + 1, w, i),
            Severity::Warning => (e, w + 1, i),
            Severity::Info => (e, w, i + 1),
        })
}

/// Stable ordering for reports: by line (unknown last), then column, then severity (errors first), then code.
pub fn sort(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        let la = a.line.unwrap_or(u32::MAX);
        let lb = b.line.unwrap_or(u32::MAX);
        la.cmp(&lb)
            .then(a.column.unwrap_or(0).cmp(&b.column.unwrap_or(0)))
            .then(b.severity.cmp(&a.severity))
            .then(a.code.cmp(&b.code))
            .then(a.path.cmp(&b.path))
    });
}

/// Path helpers for the `a.b[0].c` notation used in [`Diagnostic::path`].
pub mod path {
    /// `base.field` (or `field` when base is empty).
    pub fn field(base: &str, name: &str) -> String {
        if base.is_empty() {
            name.to_string()
        } else {
            format!("{base}.{name}")
        }
    }

    /// `base[index]`.
    pub fn index(base: &str, index: usize) -> String {
        format!("{base}[{index}]")
    }
}

/// Did-you-mean: the closest candidate to `input`, if it is close enough to be a plausible typo.
///
/// Algorithm (tuned on agent-style typos; 17/17 intended hits and 0 false positives on the evaluation set): normalize
/// the input key (`sampleRate`/`Sample-Rate`/`sample rate` → `sample_rate`), pick the candidate with the highest
/// Jaro-Winkler similarity if it is ≥ 0.88; otherwise, if the input is a whole `_`-separated segment of exactly one
/// candidate (`lufs` → `target_lufs`), suggest that candidate.
pub fn did_you_mean<'a, I>(input: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let typed = normalize_key(input.trim());
    if typed.is_empty() {
        return None;
    }
    let candidates: Vec<&'a str> = candidates.into_iter().collect();
    if let Some(exact) = candidates.iter().find(|c| c.eq_ignore_ascii_case(&typed)) {
        return Some(exact);
    }
    let best = candidates
        .iter()
        .map(|c| (*c, strsim::jaro_winkler(&typed, &c.to_ascii_lowercase())))
        .filter(|(_, score)| *score >= 0.88)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(c, _)| c);
    best.or_else(|| {
        if typed.len() < 3 {
            return None;
        }
        let mut segment_hits = candidates
            .iter()
            .filter(|c| c.split('_').any(|part| part == typed));
        match (segment_hits.next(), segment_hits.next()) {
            (Some(c), None) => Some(*c),
            _ => None,
        }
    })
}

/// Normalize a key for comparison: camelCase, `-` and spaces become snake_case.
pub fn normalize_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        match ch {
            '-' | ' ' => out.push('_'),
            c if c.is_ascii_uppercase() => {
                if i > 0 && !out.ends_with('_') {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            }
            c => out.push(c),
        }
    }
    out
}

/// A file path as it may appear in a message or hint: relative paths are kept as written, absolute paths
/// (`/…`, `\…`, `C:\…`, `C:/…`, `~/…`) are reduced to their file name.
///
/// Diagnostics end up in rendered artifacts (manifest, preview), which must not depend on the directory
/// they were rendered in (DESIGN.md §0.1), so no diagnostic may print an absolute path.
pub fn display_path(path: &str) -> String {
    let p = path.trim();
    let bytes = p.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let absolute = p.starts_with(['/', '\\']) || p.starts_with("~/") || p == "~" || drive;
    if !absolute {
        return p.to_string();
    }
    match p
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty() && *name != "~" && !name.ends_with(':'))
    {
        Some(name) => name.to_string(),
        None => p.to_string(),
    }
}

/// A duration in human units for messages: `350 ms`, `1.5 s`, `2.5 min`, `25.0 h`, `41.7 days`,
/// `31.7 years` (negative values keep their sign).
pub fn human_duration(ms: f64) -> String {
    const SECOND: f64 = 1000.0;
    const MINUTE: f64 = 60.0 * SECOND;
    const HOUR: f64 = 60.0 * MINUTE;
    const DAY: f64 = 24.0 * HOUR;
    const YEAR: f64 = 365.25 * DAY;
    if !ms.is_finite() {
        return format!("{ms} ms");
    }
    let sign = if ms < 0.0 { "-" } else { "" };
    let a = ms.abs();
    let trimmed = |v: f64, decimals: usize| {
        let s = format!("{v:.decimals$}");
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    let text = if a >= YEAR {
        let years = a / YEAR;
        if years >= 1e6 {
            format!("{years:.1e} years")
        } else {
            format!("{years:.1} years")
        }
    } else if a >= 2.0 * DAY {
        format!("{:.1} days", a / DAY)
    } else if a >= HOUR {
        let hours = format!("{:.1}", a / HOUR);
        if a > DAY && hours == "24.0" {
            "just over 24 h".to_string()
        } else {
            format!("{hours} h")
        }
    } else if a >= MINUTE {
        format!("{} min", trimmed(a / MINUTE, 1))
    } else if a >= SECOND {
        format!("{} s", trimmed(a / SECOND, 3))
    } else {
        format!("{} ms", trimmed(a, 3))
    };
    format!("{sign}{text}")
}

/// Format `items` as a short, comma-separated list of backticked names, truncated after `max` entries.
pub fn list_names<'a, I>(items: I, max: usize) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let all: Vec<&str> = items.into_iter().collect();
    let mut out: Vec<String> = all.iter().take(max).map(|s| format!("`{s}`")).collect();
    if all.len() > max {
        out.push(format!("… ({} more)", all.len() - max));
    }
    out.join(", ")
}

/// Diagnostic codes (stable API). Keep in sync with `docs/DESIGN.md` §7.
pub mod codes {
    // parse / schema
    /// The YAML text cannot be parsed (also: duplicate keys, several documents, size/nesting limits).
    pub const YAML_SYNTAX: &str = "yaml_syntax";
    /// The JSON text cannot be parsed (also: duplicate keys, size/nesting limits).
    pub const JSON_SYNTAX: &str = "json_syntax";
    /// A value has the wrong type (string instead of number, list instead of mapping, …).
    pub const INVALID_TYPE: &str = "invalid_type";
    /// A key that is not part of the DSL at that location (`x-`/`x_` keys are exempt).
    pub const UNKNOWN_FIELD: &str = "unknown_field";
    /// A required key is absent.
    pub const MISSING_FIELD: &str = "missing_field";
    /// The DSL version is not 1.x (error) or is a newer 1.x (warning).
    pub const UNSUPPORTED_VERSION: &str = "unsupported_version";
    /// A value has the right type but is not allowed (range, enum name, duration syntax, …).
    pub const INVALID_VALUE: &str = "invalid_value";
    /// A key that does not apply where it is written: to this track or provider type, or a voice-level key
    /// (`voice`, `model`, …) inside `settings` (warning).
    pub const FIELD_NOT_APPLICABLE: &str = "field_not_applicable";
    /// Keys that cannot be combined (`start_at_ms` + `start_relative`, pause + cue, …).
    pub const CONFLICTING_FIELDS: &str = "conflicting_fields";
    /// The timeline has no tracks.
    pub const EMPTY_TRACKS: &str = "empty_tracks";
    /// A speech `sequence` (or a `clips` list) is empty, or has no cues.
    pub const EMPTY_SEQUENCE: &str = "empty_sequence";
    /// A cue `text` (or `caption`) is empty.
    pub const EMPTY_TEXT: &str = "empty_text";
    /// An id is used twice in the shared track/cue/clip namespace.
    pub const DUPLICATE_ID: &str = "duplicate_id";
    /// An id does not match `^[A-Za-z0-9_][A-Za-z0-9_.:-]*$`.
    pub const INVALID_ID: &str = "invalid_id";
    /// An API key, token or password is stored in the timeline.
    pub const SECRET_IN_TIMELINE: &str = "secret_in_timeline";
    // references
    /// An anchor or `trigger_tracks` entry names an id that does not exist (or has the wrong kind).
    pub const UNKNOWN_REFERENCE: &str = "unknown_reference";
    /// An item, track or ducking configuration refers to itself.
    pub const SELF_REFERENCE: &str = "self_reference";
    /// Anchors form a cycle (`cue_02 → cue_03 → cue_02`).
    pub const DEPENDENCY_CYCLE: &str = "dependency_cycle";
    /// An anchor uses the end of a looping clip without a length.
    pub const ANCHOR_OPEN_ENDED: &str = "anchor_open_ended";
    /// A provider name that is neither configured nor built in.
    pub const UNKNOWN_PROVIDER: &str = "unknown_provider";
    /// A cue has no provider at any voice layer.
    pub const MISSING_PROVIDER: &str = "missing_provider";
    /// A cue has no `voice_id` and its provider has no default voice.
    pub const MISSING_VOICE: &str = "missing_voice";
    /// A speaker that is not in a non-empty `speakers` map: an error when it is a misspelling of a defined
    /// speaker (the did-you-mean finds one), else a warning (the speaker is only a subtitle label).
    pub const UNKNOWN_SPEAKER: &str = "unknown_speaker";
    /// A cue uses its provider's default voice (info).
    pub const DEFAULT_VOICE: &str = "default_voice";
    // output
    /// The output sample rate is out of range or not supported by the format.
    pub const UNSUPPORTED_SAMPLE_RATE: &str = "unsupported_sample_rate";
    /// The bit depth is not 16, 24 or 32 (or 32 with flac).
    pub const UNSUPPORTED_BIT_DEPTH: &str = "unsupported_bit_depth";
    /// A bitrate was given for a lossless format (warning).
    pub const BITRATE_IGNORED: &str = "bitrate_ignored";
    /// An mp3 bitrate was rounded to a supported constant bitrate (info).
    pub const BITRATE_ADJUSTED: &str = "bitrate_adjusted";
    /// The explicit format and the output path extension disagree (warning).
    pub const FORMAT_EXTENSION_MISMATCH: &str = "format_extension_mismatch";
    /// The output format needs ffmpeg, which is not available (engine).
    pub const FORMAT_REQUIRES_FFMPEG: &str = "format_requires_ffmpeg";
    // environment (engine)
    /// An audio source file does not exist (engine).
    pub const ASSET_NOT_FOUND: &str = "asset_not_found";
    /// An audio source file cannot be read or decoded (engine).
    pub const ASSET_UNREADABLE: &str = "asset_unreadable";
    /// A provider's API key env var is not set (engine).
    pub const MISSING_CREDENTIALS: &str = "missing_credentials";
    /// A provider cannot be used in this environment (engine).
    pub const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";
    /// A cue's text exceeds the provider's request limit (engine).
    pub const TEXT_TOO_LONG: &str = "text_too_long";
    // layout
    /// Speech cues overlap (layout).
    pub const SPEECH_OVERLAP: &str = "speech_overlap";
    /// A cue was moved later by the `push` overlap policy (layout, info).
    pub const CUE_PUSHED: &str = "cue_pushed";
    /// A resolved start before 0 was clamped to 0 (layout).
    pub const NEGATIVE_START: &str = "negative_start";
    /// Content beyond `output.duration_ms` is cut (layout).
    pub const CONTENT_TRUNCATED: &str = "content_truncated";
    /// A non-looping clip is shorter than its requested length (layout, info).
    pub const CLIP_SHORTER_THAN_REQUESTED: &str = "clip_shorter_than_requested";
    /// A clip's fades were longer than the clip and were scaled down (layout, info).
    pub const FADES_SCALED: &str = "fades_scaled";
    /// An open-ended bed has no fade-out (layout, info).
    pub const NO_FADE_OUT_ON_BED: &str = "no_fade_out_on_bed";
    /// A muted trigger track is ignored by sidechain ducking (layout, info).
    pub const MUTED_TRIGGER: &str = "muted_trigger";
    // render
    /// A TTS request failed (render).
    pub const SYNTHESIS_FAILED: &str = "synthesis_failed";
    /// A source was too short or quiet to measure its loudness (render, info).
    pub const NORMALIZATION_SKIPPED: &str = "normalization_skipped";
    /// The master loudness target cannot be reached (render).
    pub const LOUDNESS_TARGET_UNREACHABLE: &str = "loudness_target_unreachable";
    /// The master is silent (render).
    pub const SILENT_OUTPUT: &str = "silent_output";
    /// Word timings were estimated instead of provided (render, info).
    pub const ALIGNMENT_ESTIMATED: &str = "alignment_estimated";
    /// Writing the output file failed (render).
    pub const ENCODE_FAILED: &str = "encode_failed";
    /// Decoding audio failed (render).
    pub const DECODE_FAILED: &str = "decode_failed";
    /// A provider ignored or replaced a setting, e.g. a model without `style`, or an output format the
    /// plan does not allow (validate/render, info).
    pub const SETTING_IGNORED: &str = "setting_ignored";
    // added in v0.1 hardening
    /// The timeline is longer than SonicCraft renders (more than 24 h, error), or its render would need
    /// more memory than the render budget (error at render, warning in dry runs).
    pub const TIMELINE_TOO_LONG: &str = "timeline_too_long";
    /// A track anchor resolves to the track's first/last declared item, but another item of that track
    /// starts earlier or ends later (layout, warning).
    pub const TRACK_ANCHOR_OUT_OF_ORDER: &str = "track_anchor_out_of_order";
    /// Mastering stopped outside the loudness tolerance without the target being unreachable (render,
    /// warning).
    pub const LOUDNESS_TARGET_MISSED: &str = "loudness_target_missed";
    /// The encoded (lossy) file still exceeds the true-peak ceiling after codec compensation (render,
    /// warning).
    pub const TRUE_PEAK_EXCEEDED: &str = "true_peak_exceeded";
    /// The TTS cache could not be written, so later renders synthesize again (render, warning).
    pub const CACHE_WRITE_FAILED: &str = "cache_write_failed";
    /// Two or more speakers resolve to the same voice (warning).
    pub const SPEAKERS_SHARE_VOICE: &str = "speakers_share_voice";
    /// An unquoted ` #` started a YAML comment inside a cue's `text` or `caption`, cutting it short
    /// (warning).
    pub const COMMENT_IN_TEXT: &str = "comment_in_text";
    /// A voice that the provider does not have here, e.g. an uninstalled macOS voice or a missing piper
    /// model (engine, error).
    pub const UNKNOWN_VOICE: &str = "unknown_voice";
    /// A value that is valid but very likely a mistake, e.g. seconds written as milliseconds (warning).
    pub const SUSPICIOUS_VALUE: &str = "suspicious_value";
    /// Damaged audio was repaired, e.g. non-finite samples replaced with silence (render, warning).
    pub const AUDIO_REPAIRED: &str = "audio_repaired";
    /// A source with more than two channels was downmixed (render, info).
    pub const CHANNELS_DOWNMIXED: &str = "channels_downmixed";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn did_you_mean_agent_typos() {
        let fields = [
            "id",
            "type",
            "fade_in_ms",
            "fade_out_ms",
            "trigger_tracks",
            "voice_id",
            "pause_duration",
            "source",
            "volume",
            "start_relative",
            "start_at_ms",
        ];
        assert_eq!(did_you_mean("fade_in", fields), Some("fade_in_ms"));
        assert_eq!(did_you_mean("triggers", fields), Some("trigger_tracks"));
        assert_eq!(did_you_mean("voice", fields), Some("voice_id"));
        assert_eq!(
            did_you_mean("pause_duraton", fields),
            Some("pause_duration")
        );
        assert_eq!(did_you_mean("start_at", fields), Some("start_at_ms"));
        assert_eq!(did_you_mean("zzzzqq", fields), None);
        assert_eq!(
            did_you_mean("sampleRate", ["sample_rate", "format"]),
            Some("sample_rate")
        );
        assert_eq!(
            did_you_mean("lufs", ["target_lufs", "true_peak_db"]),
            Some("target_lufs")
        );
        // Plausible-looking but wrong suggestions must not be offered.
        assert_eq!(did_you_mean("speaker", ["speed", "volume"]), None);
        assert_eq!(did_you_mean("banana", fields), None);
    }

    #[test]
    fn severity_order() {
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
    }

    #[test]
    fn related_ids_are_deduplicated() {
        let d = Diagnostic::warning("x", "m")
            .with_related(["bed", "bed"])
            .with_related(["cue", "bed"]);
        assert_eq!(d.related, ["bed", "cue"]);
    }

    #[test]
    fn display_paths_never_show_directories_of_absolute_paths() {
        assert_eq!(display_path("out/demo.mp3"), "out/demo.mp3");
        assert_eq!(display_path("../renders/demo.mp3"), "../renders/demo.mp3");
        assert_eq!(display_path("demo.wav"), "demo.wav");
        assert_eq!(display_path("/private/tmp/a/out/demo.mp3"), "demo.mp3");
        assert_eq!(display_path("C:\\Users\\me\\demo.mp3"), "demo.mp3");
        assert_eq!(display_path("C:/renders/demo.mp3"), "demo.mp3");
        assert_eq!(display_path("\\\\server\\share\\demo.mp3"), "demo.mp3");
        assert_eq!(display_path("~/Music/demo.flac"), "demo.flac");
        assert_eq!(display_path("/tmp/out/"), "out");
    }

    #[test]
    fn human_durations() {
        assert_eq!(human_duration(350.0), "350 ms");
        assert_eq!(human_duration(2.5), "2.5 ms");
        assert_eq!(human_duration(1500.0), "1.5 s");
        assert_eq!(human_duration(150_000.0), "2.5 min");
        assert_eq!(human_duration(90_000_000.0), "25.0 h");
        assert_eq!(human_duration(86_400_001.0), "just over 24 h");
        assert_eq!(human_duration(3.6e9), "41.7 days");
        assert_eq!(human_duration(1e12), "31.7 years");
        assert_eq!(human_duration(1.7e12), "53.9 years");
        assert_eq!(human_duration(-1e12), "-31.7 years");
        assert_eq!(human_duration(1e18), "3.2e7 years");
    }
}
