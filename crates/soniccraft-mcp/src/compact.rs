//! Compact reports: bounded projections of the engine reports for results that would otherwise be too
//! large for an agent's context (a 400-cue dry run is about 200,000 characters of JSON).
//!
//! A compact report keeps the shape of the full report (the same keys, so code reading a PlanReport or
//! RenderReport still works) but caps every list that grows with the timeline, and says what it left out:
//!
//! * `items`: the first and last items plus the items a kept diagnostic names and the cues the overlap
//!   policy moved; `items_total` and `items_truncated`;
//! * `diagnostics`: errors first, then warnings (infos only in shortened full reports), in document order;
//!   `diagnostics_total`, `diagnostic_counts` and `diagnostics_truncated`;
//! * `tracks`, `ducking` (and each track's `regions`), `files.stems`, `voices`: capped, with a `*_total`;
//! * `diagram`: whole when short, else without its cue and clip tables, else left out (`diagram_truncated`);
//! * `detail` (`summary`, or `full` for a full report shortened to fit the message size limit) and
//!   `full_report`, which says where the complete report is.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{Map, Value, json};
use soniccraft_engine::{
    Diagnostic, DuckingReport, ItemReport, PlanReport, RenderFailure, RenderReport, Severity,
    TrackReport, ValidationReport, VoicesReport,
};

/// Most `related` ids kept per diagnostic (a grouped diagnostic can relate thousands of cues).
const RELATED_IDS: usize = 25;

/// How much of each growing list a compact report keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caps {
    /// Items kept at each end of `items`.
    pub(crate) edge_items: usize,
    /// Further items kept because a diagnostic names them or the overlap policy moved them.
    pub(crate) notable_items: usize,
    /// Longest cue text kept in `items[].text`, in characters (0 drops the text).
    pub(crate) item_text: usize,
    /// Most diagnostics kept.
    pub(crate) diagnostics: usize,
    /// Whether infos may fill the diagnostics left after errors and warnings.
    pub(crate) infos: bool,
    /// Most tracks kept.
    pub(crate) tracks: usize,
    /// Most ducked tracks kept in `ducking`.
    pub(crate) ducking: usize,
    /// Most regions kept per ducked track.
    pub(crate) regions: usize,
    /// Most stems kept in `files.stems`.
    pub(crate) stems: usize,
    /// Most voices kept in a voices report.
    pub(crate) voices: usize,
    /// Longest diagram kept, in characters (0 drops it).
    pub(crate) diagram: usize,
}

impl Caps {
    /// The compact report of `detail: "summary"` (and of `auto` for large reports).
    pub(crate) const STANDARD: Caps = Caps {
        edge_items: 10,
        notable_items: 40,
        item_text: 80,
        diagnostics: 50,
        infos: false,
        tracks: 50,
        ducking: 20,
        regions: 20,
        stems: 50,
        voices: 100,
        diagram: 6_000,
    };

    /// A smaller compact report, when the standard one does not fit the result budget.
    pub(crate) const REDUCED: Caps = Caps {
        edge_items: 5,
        notable_items: 10,
        item_text: 40,
        diagnostics: 20,
        infos: false,
        tracks: 20,
        ducking: 10,
        regions: 5,
        stems: 20,
        voices: 40,
        diagram: 3_000,
    };

    /// The smallest compact report that still lists anything.
    pub(crate) const TINY: Caps = Caps {
        edge_items: 3,
        notable_items: 0,
        item_text: 0,
        diagnostics: 8,
        infos: false,
        tracks: 8,
        ducking: 4,
        regions: 3,
        stems: 8,
        voices: 15,
        diagram: 0,
    };

    /// A full report shortened to fit the protocol's message size limit (`detail: "full"`).
    pub(crate) const LARGE: Caps = Caps {
        edge_items: 2_000,
        notable_items: 4_000,
        item_text: 200,
        diagnostics: 4_000,
        infos: true,
        tracks: 2_000,
        ducking: 500,
        regions: 2_000,
        stems: 2_000,
        voices: 4_000,
        diagram: 0,
    };

    /// The next step down from [`Caps::LARGE`].
    pub(crate) const MEDIUM: Caps = Caps {
        edge_items: 300,
        notable_items: 600,
        item_text: 80,
        diagnostics: 600,
        infos: true,
        tracks: 300,
        ducking: 100,
        regions: 300,
        stems: 300,
        voices: 600,
        diagram: 0,
    };
}

/// The `detail` label and the `full_report` note a compact report carries.
#[derive(Debug, Clone)]
pub(crate) struct Labels {
    /// `summary` (a compact report) or `full` (a full report shortened to fit).
    pub(crate) detail: &'static str,
    /// Where the complete report is.
    pub(crate) full_report: String,
}

fn value<T: Serialize + ?Sized>(item: &T) -> Value {
    // The engine's report types always serialize (string keys, finite numbers or null).
    serde_json::to_value(item).unwrap_or(Value::Null)
}

/// Compact ValidationReport.
pub(crate) fn validation(report: &ValidationReport, caps: &Caps, labels: &Labels) -> Value {
    let mut map = Map::new();
    map.insert("valid".into(), json!(report.valid));
    put_diagnostics(&mut map, &report.diagnostics, caps);
    if let Some(summary) = &report.summary {
        map.insert("summary".into(), value(summary));
    }
    finish(map, labels)
}

/// Compact PlanReport.
pub(crate) fn plan(report: &PlanReport, caps: &Caps, labels: &Labels) -> Value {
    let mut map = Map::new();
    map.insert("valid".into(), json!(report.valid));
    let kept = put_diagnostics(&mut map, &report.diagnostics, caps);
    map.insert("duration_ms".into(), json!(report.duration_ms));
    map.insert("estimated".into(), json!(report.estimated));
    map.insert("sample_rate".into(), json!(report.sample_rate));
    put_tracks(&mut map, &report.tracks, caps);
    put_items(&mut map, &report.items, &kept, caps);
    put_ducking(&mut map, &report.ducking, caps);
    map.insert("synthesis".into(), value(&report.synthesis));
    put_diagram(&mut map, &report.diagram, caps);
    finish(map, labels)
}

/// Compact RenderReport.
pub(crate) fn render(report: &RenderReport, caps: &Caps, labels: &Labels) -> Value {
    let mut map = Map::new();
    map.insert("output".into(), value(&report.output));
    map.insert("loudness".into(), value(&report.loudness));
    let mut files = value(&report.files);
    if let Some(stems) = files.get_mut("stems").and_then(Value::as_array_mut)
        && stems.len() > caps.stems
    {
        let total = stems.len();
        stems.truncate(caps.stems);
        files["stems_total"] = json!(total);
    }
    map.insert("files".into(), files);
    put_tracks(&mut map, &report.tracks, caps);
    let kept = select_diagnostics(&report.diagnostics, caps);
    put_items(&mut map, &report.items, &kept, caps);
    put_ducking(&mut map, &report.ducking, caps);
    map.insert("synthesis".into(), value(&report.synthesis));
    put_diagnostics(&mut map, &report.diagnostics, caps);
    if !report.timings_ms.is_empty() {
        map.insert("timings_ms".into(), value(&report.timings_ms));
    }
    finish(map, labels)
}

/// Compact RenderFailure.
pub(crate) fn render_failure(failure: &RenderFailure, caps: &Caps, labels: &Labels) -> Value {
    let mut map = Map::new();
    map.insert("message".into(), json!(failure.message));
    put_diagnostics(&mut map, &failure.diagnostics, caps);
    if failure.cancelled {
        map.insert("cancelled".into(), json!(true));
    }
    finish(map, labels)
}

/// Compact VoicesReport.
pub(crate) fn voices(report: &VoicesReport, caps: &Caps, labels: &Labels) -> Value {
    let mut map = Map::new();
    map.insert("provider".into(), json!(report.provider));
    let kept = report.voices.len().min(caps.voices);
    map.insert("voices".into(), value(&report.voices[..kept]));
    map.insert("voices_total".into(), json!(report.voices.len()));
    map.insert("voices_truncated".into(), json!(kept < report.voices.len()));
    finish(map, labels)
}

fn finish(mut map: Map<String, Value>, labels: &Labels) -> Value {
    map.insert("detail".into(), json!(labels.detail));
    map.insert("full_report".into(), json!(labels.full_report));
    Value::Object(map)
}

/// The diagnostics a compact report keeps: errors first, then warnings, then (when allowed) infos, up to
/// `caps.diagnostics`, returned in report (document) order.
pub(crate) fn select_diagnostics<'a>(all: &'a [Diagnostic], caps: &Caps) -> Vec<&'a Diagnostic> {
    let mut chosen = vec![false; all.len()];
    let mut left = caps.diagnostics;
    let severities: &[Severity] = if caps.infos {
        &[Severity::Error, Severity::Warning, Severity::Info]
    } else {
        &[Severity::Error, Severity::Warning]
    };
    for severity in severities {
        for (i, d) in all.iter().enumerate() {
            if left == 0 {
                break;
            }
            if d.severity == *severity {
                chosen[i] = true;
                left -= 1;
            }
        }
    }
    all.iter()
        .zip(chosen)
        .filter_map(|(d, keep)| keep.then_some(d))
        .collect()
}

/// `diagnostics` (selected), `diagnostics_total`, `diagnostic_counts` and `diagnostics_truncated`; returns
/// the kept diagnostics.
fn put_diagnostics<'a>(
    map: &mut Map<String, Value>,
    all: &'a [Diagnostic],
    caps: &Caps,
) -> Vec<&'a Diagnostic> {
    let kept = select_diagnostics(all, caps);
    let (errors, warnings, infos) = soniccraft_core::diagnostics::counts(all);
    let values: Vec<Value> = kept.iter().map(|d| diagnostic_value(d)).collect();
    map.insert("diagnostics".into(), Value::Array(values));
    map.insert("diagnostics_total".into(), json!(all.len()));
    map.insert(
        "diagnostic_counts".into(),
        json!({"errors": errors, "warnings": warnings, "infos": infos}),
    );
    map.insert(
        "diagnostics_truncated".into(),
        json!(kept.len() < all.len()),
    );
    kept
}

/// One diagnostic, its `related` ids capped at [`RELATED_IDS`] (`related_total` gives the count).
fn diagnostic_value(d: &Diagnostic) -> Value {
    let mut v = value(d);
    if d.related.len() > RELATED_IDS {
        v["related"] = value(&d.related[..RELATED_IDS]);
        v["related_total"] = json!(d.related.len());
    }
    v
}

/// Item ids a diagnostic names: its `related` ids and the backquoted words of its message and hint.
fn mentioned_ids(diagnostics: &[&Diagnostic]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for d in diagnostics {
        ids.extend(d.related.iter().cloned());
        for text in std::iter::once(d.message.as_str()).chain(d.hint.as_deref()) {
            for (i, part) in text.split('`').enumerate() {
                if i % 2 == 1 && !part.is_empty() && !part.contains(char::is_whitespace) {
                    ids.insert(part.to_string());
                }
            }
        }
    }
    ids
}

/// Indices of the items a compact report keeps, in report order: the first and last
/// `caps.edge_items`, then (up to `caps.notable_items`) the items a kept diagnostic names, then the cues
/// the overlap policy moved.
pub(crate) fn select_items(
    items: &[ItemReport],
    diagnostics: &[&Diagnostic],
    caps: &Caps,
) -> Vec<usize> {
    let n = items.len();
    let mut keep = vec![false; n];
    for i in (0..caps.edge_items.min(n)).chain(n.saturating_sub(caps.edge_items)..n) {
        keep[i] = true;
    }
    let mentioned = mentioned_ids(diagnostics);
    let mut left = caps.notable_items;
    let passes: [&dyn Fn(&ItemReport) -> bool; 2] =
        [&|item| mentioned.contains(item.id.as_str()), &|item| {
            item.pushed_from_ms.is_some()
        }];
    for notable in passes {
        for (i, item) in items.iter().enumerate() {
            if left == 0 {
                break;
            }
            if !keep[i] && notable(item) {
                keep[i] = true;
                left -= 1;
            }
        }
    }
    (0..n).filter(|&i| keep[i]).collect()
}

fn put_items(
    map: &mut Map<String, Value>,
    items: &[ItemReport],
    diagnostics: &[&Diagnostic],
    caps: &Caps,
) {
    let kept = select_items(items, diagnostics, caps);
    let values: Vec<Value> = kept
        .iter()
        .map(|&i| item_value(&items[i], caps.item_text))
        .collect();
    map.insert("items".into(), Value::Array(values));
    map.insert("items_total".into(), json!(items.len()));
    map.insert("items_truncated".into(), json!(kept.len() < items.len()));
}

/// One item, its cue text cut to `max_text` characters (dropped when 0).
fn item_value(item: &ItemReport, max_text: usize) -> Value {
    let mut v = value(item);
    if let Some(object) = v.as_object_mut()
        && let Some(Value::String(text)) = object.get("text")
    {
        if max_text == 0 {
            object.remove("text");
        } else if text.chars().count() > max_text {
            let cut = shorten(text, max_text);
            object.insert("text".into(), Value::String(cut));
        }
    }
    v
}

/// `text` cut to at most `max` characters, ending in `…` when cut.
pub(crate) fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn put_tracks(map: &mut Map<String, Value>, tracks: &[TrackReport], caps: &Caps) {
    let kept = tracks.len().min(caps.tracks);
    map.insert("tracks".into(), value(&tracks[..kept]));
    if kept < tracks.len() {
        map.insert("tracks_total".into(), json!(tracks.len()));
    }
}

fn put_ducking(map: &mut Map<String, Value>, ducking: &[DuckingReport], caps: &Caps) {
    let kept: Vec<Value> = ducking
        .iter()
        .take(caps.ducking)
        .map(|duck| {
            let mut v = value(duck);
            if duck.regions.len() > caps.regions {
                v["regions"] = value(&duck.regions[..caps.regions]);
                v["regions_total"] = json!(duck.regions.len());
            }
            v
        })
        .collect();
    map.insert("ducking".into(), Value::Array(kept));
    if ducking.len() > caps.ducking {
        map.insert("ducking_total".into(), json!(ducking.len()));
    }
}

fn put_diagram(map: &mut Map<String, Value>, diagram: &str, caps: &Caps) {
    let (shown, truncated) = fit_diagram(diagram, caps.diagram);
    map.insert("diagram".into(), json!(shown));
    if truncated {
        map.insert("diagram_truncated".into(), json!(true));
    }
}

/// The diagram if it has at most `max` characters, else its head (without the cue and clip tables) if
/// that fits, else nothing; and whether anything was left out.
pub(crate) fn fit_diagram(diagram: &str, max: usize) -> (&str, bool) {
    let full = diagram.trim_end();
    if full.is_empty() {
        return ("", false);
    }
    if full.chars().count() <= max {
        return (diagram, false);
    }
    let head = diagram_head(full);
    if !head.is_empty() && head.chars().count() <= max {
        (head, true)
    } else {
        ("", true)
    }
}

/// The plan diagram without its cue and clip tables: the header, the time ruler, the track lanes and the
/// legend (see `soniccraft_layout::render_diagram`; the tables start at a `<n> cues:` or `<n> clips:`
/// line).
pub(crate) fn diagram_head(diagram: &str) -> &str {
    let mut offset = 0;
    for line in diagram.split_inclusive('\n') {
        if is_table_header(line.trim_end()) {
            return diagram[..offset].trim_end();
        }
        offset += line.len();
    }
    diagram.trim_end()
}

fn is_table_header(line: &str) -> bool {
    match line.split_once(' ') {
        Some((n, rest)) => {
            !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && matches!(rest, "cue:" | "cues:" | "clip:" | "clips:")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reports are built from JSON, as clients see them, so the engine can add fields freely.
    fn from_json<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).expect("a valid report")
    }

    fn cue(i: usize, pushed: bool) -> ItemReport {
        let mut item = json!({
            "id": format!("c{i:03}"),
            "kind": "cue",
            "track_id": "talk",
            "start_ms": i as f64 * 1000.0,
            "end_ms": i as f64 * 1000.0 + 900.0,
            "duration_ms": 900.0,
            "duration_source": "estimate",
            "provider": "mock",
            "text": format!("Line number {i} of a long conversation about nothing much."),
        });
        if pushed {
            item["pushed_from_ms"] = json!(i as f64 * 1000.0 - 100.0);
        }
        from_json(item)
    }

    fn labels() -> Labels {
        Labels {
            detail: "summary",
            full_report: "call again with detail: \"full\"".into(),
        }
    }

    #[test]
    fn items_keep_the_ends_the_named_and_the_moved() {
        let items: Vec<ItemReport> = (0..100).map(|i| cue(i, i == 50 || i == 70)).collect();
        let warning = Diagnostic::warning("speech_overlap", "`c040` overlaps `c041`")
            .with_related(vec!["c060".to_string()]);
        let kept = select_items(&items, &[&warning], &Caps::STANDARD);
        let ids: Vec<&str> = kept.iter().map(|&i| items[i].id.as_str()).collect();
        assert_eq!(
            &ids[..10],
            (0..10).map(|i| format!("c{i:03}")).collect::<Vec<_>>()
        );
        for id in ["c040", "c041", "c060", "c050", "c070", "c090", "c099"] {
            assert!(ids.contains(&id), "{id} missing from {ids:?}");
        }
        assert_eq!(kept.len(), 25);
        // Report order is kept.
        assert!(kept.windows(2).all(|w| w[0] < w[1]));
        // Named items come before moved ones when the cap is tight.
        let tight = Caps {
            notable_items: 2,
            ..Caps::STANDARD
        };
        let kept = select_items(&items, &[&warning], &tight);
        let ids: Vec<&str> = kept.iter().map(|&i| items[i].id.as_str()).collect();
        assert!(ids.contains(&"c040") && ids.contains(&"c041") && !ids.contains(&"c050"));
    }

    #[test]
    fn diagnostics_put_errors_first_and_count_everything() {
        let mut all: Vec<Diagnostic> = (0..30)
            .map(|i| Diagnostic::info("cue_pushed", format!("info {i}")))
            .collect();
        all.extend((0..60).map(|i| Diagnostic::warning("w", format!("warning {i}"))));
        all.push(
            Diagnostic::error("e", "the error").with_related((0..30).map(|i| format!("c{i}"))),
        );
        let report: ValidationReport = from_json(json!({"valid": false, "diagnostics": all}));
        let v = validation(&report, &Caps::STANDARD, &labels());
        let kept = v["diagnostics"].as_array().unwrap();
        assert_eq!(kept.len(), 50);
        assert_eq!(
            kept.last().unwrap()["related"].as_array().unwrap().len(),
            25
        );
        assert_eq!(kept.last().unwrap()["related_total"], 30);
        assert_eq!(kept.last().unwrap()["code"], "e", "errors are always kept");
        assert!(kept.iter().all(|d| d["severity"] != "info"));
        assert_eq!(v["diagnostics_total"], 91);
        assert_eq!(
            v["diagnostic_counts"],
            json!({"errors": 1, "warnings": 60, "infos": 30})
        );
        assert_eq!(v["diagnostics_truncated"], true);
        assert_eq!(v["detail"], "summary");
        assert!(v["full_report"].as_str().unwrap().contains("full"));
    }

    #[test]
    fn plan_lists_are_capped_with_totals() {
        let items: Vec<ItemReport> = (0..400).map(|i| cue(i, false)).collect();
        let tracks: Vec<Value> = (0..60)
            .map(|i| json!({"id": format!("t{i}"), "type": "speech", "gain_db": 0.0, "pan": 0.0, "mute": false}))
            .collect();
        let regions: Vec<Value> = (0..100)
            .map(|i| json!({"start_ms": i as f64, "end_ms": i as f64 + 0.5}))
            .collect();
        let report: PlanReport = from_json(json!({
            "valid": true,
            "diagnostics": [],
            "duration_ms": 400_000.0,
            "estimated": true,
            "sample_rate": 8000,
            "tracks": tracks,
            "items": items,
            "ducking": [{
                "track_id": "bed",
                "mode": "sidechain",
                "reduction_db": 12.0,
                "triggers": ["talk"],
                "regions": regions,
            }],
            "synthesis": {"cues": 400, "cached": 0, "characters_to_synthesize": 23_090, "by_provider": {}},
            "diagram": "x".repeat(20_000),
        }));
        let v = plan(&report, &Caps::STANDARD, &labels());
        assert_eq!(v["items"].as_array().unwrap().len(), 20);
        assert_eq!(v["items_total"], 400);
        assert_eq!(v["items_truncated"], true);
        assert_eq!(v["tracks"].as_array().unwrap().len(), 50);
        assert_eq!(v["tracks_total"], 60);
        assert_eq!(v["ducking"][0]["regions"].as_array().unwrap().len(), 20);
        assert_eq!(v["ducking"][0]["regions_total"], 100);
        assert_eq!(v["diagram"], "");
        assert_eq!(v["diagram_truncated"], true);
        let text = v["items"][0]["text"].as_str().unwrap();
        assert!(text.chars().count() <= 80, "{text}");
        assert!(
            v.to_string().len() < 12_000,
            "{} bytes",
            v.to_string().len()
        );
    }

    #[test]
    fn diagram_head_stops_before_the_tables() {
        let diagram = "SonicCraft plan: x\nduration 0:12.4\n\ntime  0s\nhost  ####\nlegend: # speech\n\n\
                       3 cues:\n  start  end\n  0:00  0:04\n\n1 clip:\n  start\n";
        assert_eq!(
            diagram_head(diagram),
            "SonicCraft plan: x\nduration 0:12.4\n\ntime  0s\nhost  ####\nlegend: # speech"
        );
        assert_eq!(fit_diagram(diagram, 10_000), (diagram, false));
        let (head, cut) = fit_diagram(diagram, 80);
        assert!(cut && head.ends_with("legend: # speech"), "{head:?}");
        assert_eq!(fit_diagram(diagram, 10), ("", true));
        assert_eq!(diagram_head("no tables here\n"), "no tables here");
        assert!(
            !is_table_header("2 cues: x")
                && !is_table_header("cues:")
                && is_table_header("12 clips:")
        );
    }

    #[test]
    fn shorten_counts_characters() {
        assert_eq!(shorten("héllo wörld", 6), "héllo…");
        assert_eq!(shorten("short", 6), "short");
    }
}
