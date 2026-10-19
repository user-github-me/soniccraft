//! The two front ends accept the same documents: the same value budget and nesting limit for JSON and YAML,
//! a dedicated diagnostic for comments in JSON, and the auto-format rules (stdin, MCP `yaml_content`).

use soniccraft_core::parse::{MAX_NESTING_DEPTH, MAX_NODES, Parsed, parse_timeline};
use soniccraft_core::{LowerOptions, SourceFormat, compile};

fn syntax_errors(p: &Parsed) -> Vec<String> {
    p.diagnostics
        .iter()
        .filter(|d| d.code == "json_syntax" || d.code == "yaml_syntax")
        .map(|d| d.message.clone())
        .collect()
}

/// `{"x-big": [0, 0, …]}` with `nodes` values in total: the root mapping, its key, the list and its items.
fn document_with_nodes(nodes: usize) -> String {
    format!("{{\"x-big\": [{}]}}", vec!["0"; nodes - 3].join(", "))
}

#[test]
fn json_and_yaml_share_the_value_budget() {
    for (nodes, accepted) in [(MAX_NODES, true), (MAX_NODES + 1, false)] {
        let text = document_with_nodes(nodes);
        for format in [SourceFormat::Json, SourceFormat::Yaml] {
            let p = parse_timeline(&text, format);
            let errors = syntax_errors(&p);
            if accepted {
                assert!(
                    errors.is_empty(),
                    "{format:?} with {nodes} nodes: {errors:?}"
                );
            } else {
                assert_eq!(
                    errors,
                    ["the document has too many values (more than 250000 nodes)"],
                    "{format:?} with {nodes} nodes"
                );
                let d = &p.diagnostics[0];
                assert!(d.line.is_some() && d.hint.is_some(), "{d:?}");
            }
        }
    }
}

#[test]
fn json_and_yaml_share_the_nesting_limit() {
    // The root mapping is level 1.
    let nested = |levels: usize| {
        format!(
            "{{\"x-deep\": {}{}}}",
            "[".repeat(levels - 1),
            "]".repeat(levels - 1)
        )
    };
    for (levels, accepted) in [(MAX_NESTING_DEPTH, true), (MAX_NESTING_DEPTH + 1, false)] {
        let text = nested(levels);
        for format in [SourceFormat::Json, SourceFormat::Yaml] {
            let errors = syntax_errors(&parse_timeline(&text, format));
            assert_eq!(
                errors.is_empty(),
                accepted,
                "{format:?} at {levels} levels: {errors:?}"
            );
            if !accepted {
                assert!(errors[0].contains("nested too deeply"), "{errors:?}");
            }
        }
    }
}

#[test]
fn json_comments_get_a_comment_diagnostic() {
    let src = "{\n  \"version\": \"1.0\",\n  // narration track\n  \"tracks\": []\n}";
    for format in [SourceFormat::Json, SourceFormat::Auto] {
        let p = parse_timeline(src, format);
        assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
        let d = &p.diagnostics[0];
        assert_eq!(d.code, "json_syntax");
        assert_eq!(d.message, "JSON does not allow comments");
        assert_eq!(
            d.hint.as_deref(),
            Some(
                "Delete the comment (use an `x-note` key, or write the timeline as YAML where `#` comments work)"
            )
        );
        assert_eq!((d.line, d.column), (Some(3), Some(3)), "{format:?}");
    }
    // After a value on the same line (the reviewer's `jcomment.json`).
    let p = parse_timeline(
        "{\n  \"version\": \"1.0\", // comment\n  \"tracks\": []\n}",
        SourceFormat::Json,
    );
    assert_eq!(
        (p.diagnostics[0].line, p.diagnostics[0].column),
        (Some(2), Some(21))
    );
}

#[test]
fn auto_format_falls_back_to_yaml_only_for_flow_style() {
    let auto = |text: &str| parse_timeline(text, SourceFormat::Auto);
    // JSON with trailing commas or comments gets the JSON diagnostic, as it does from a .json file.
    let trailing = "{\"version\": \"1.0\", \"tracks\": [],}";
    assert_eq!(auto(trailing).diagnostics[0].code, "json_syntax");
    assert_eq!(
        auto(trailing).diagnostics[0].message,
        parse_timeline(trailing, SourceFormat::Json).diagnostics[0].message
    );
    let hash = "{\"version\": \"1.0\", # comment\n \"tracks\": []}";
    assert_eq!(
        auto(hash).diagnostics[0].message,
        "JSON does not allow comments"
    );
    // YAML flow style (unquoted keys) is YAML, with or without `#` comments.
    for flow in [
        "{version: \"1.0\", tracks: []}",
        "{version: \"1.0\", # a YAML comment\n tracks: []}",
        "{'version': '1.0', tracks: []}",
    ] {
        let p = auto(flow);
        assert!(syntax_errors(&p).is_empty(), "{flow}: {:?}", p.diagnostics);
        assert_eq!(
            p.value.as_ref().map(|v| v["version"].clone()),
            Some(serde_json::json!("1.0"))
        );
    }
    // The comment is never merged into a key (the reviewer's `j1.json` on stdin).
    let c = compile(
        "{\n  \"version\": \"1.0\",\n  // a comment\n  \"tracks\": []\n}",
        SourceFormat::Auto,
        &LowerOptions::default(),
    );
    assert_eq!(c.diagnostics.len(), 1, "{:?}", c.diagnostics);
    assert_eq!(c.diagnostics[0].code, "json_syntax");
}

/// The hand-written scanners (JSON pre-scan and spans, comment detection, secret ranges) never panic:
/// thousands of corrupted documents, from a fixed-seed generator, through every front end.
#[test]
fn corrupted_documents_never_panic() {
    let yaml = "version: \"1.0\"\nproviders:\n  k: {type: kokoro, base_url: \"http://u:pw0123456789@h:9\"}\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - text: Tip #1: quote me\n        x-n: [1, {a: 2}]\n      - pause: 300ms\n";
    let json = "{\"version\": \"1.0\", \"x-a\": [1, {\"b\": \"\\u00e9\"}], \"tracks\": [{\"id\": \"v\", \"type\": \"speech\", \"sequence\": [{\"text\": \"sk-proj-AbCdEfGhIjKlMnOpQrStUvWx\"}]}]}";
    let pieces = [
        "{", "}", "[", "]", ":", ",", "\"", "'", "#", "//", "/*", "*/", "\\", "\\u", "\n", " ",
        "-", "0", "1e9", "x-", "é", "\u{feff}", "&a", "*a", "<<", "---", "\t", "sk-", "Bearer ",
        "@", "://",
    ];
    let mut state: u64 = 0x5eed_5eed_5eed_5eed;
    let mut next = |bound: usize| {
        // xorshift64*: deterministic, no dependency.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % bound
    };
    for round in 0..3000 {
        let mut doc: Vec<char> = if round % 2 == 0 { yaml } else { json }.chars().collect();
        for _ in 0..1 + next(4) {
            let at = next(doc.len() + 1);
            match next(3) {
                0 if at < doc.len() => {
                    doc.remove(at);
                }
                _ => {
                    for (i, c) in pieces[next(pieces.len())].chars().enumerate() {
                        doc.insert(at + i, c);
                    }
                }
            }
        }
        let text: String = doc.into_iter().collect();
        for format in [SourceFormat::Yaml, SourceFormat::Json, SourceFormat::Auto] {
            let parsed = parse_timeline(&text, format);
            for r in soniccraft_core::parse::secret_spans(&text, format) {
                assert!(r.start < r.end && r.end <= text.len() && text.is_char_boundary(r.start));
            }
            if parsed.timeline.is_some() {
                let _ = compile(&text, format, &LowerOptions::default());
            }
        }
    }
}

#[test]
fn json_annotation_subtrees_are_not_indexed() {
    let items = vec!["{\"a\": [1, 2, 3]}"; 2000].join(", ");
    let text = format!(
        "{{\"version\": \"1.0\", \"x-big\": [{items}], \"tracks\": [{{\"id\": \"v\", \"type\": \"speech\", \"x_n\": [{items}], \
         \"sequence\": [{{\"text\": \"hi\", \"provider\": \"mock\"}}]}}]}}"
    );
    let p = parse_timeline(&text, SourceFormat::Json);
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    assert!(p.spans.len() < 40, "{} spans", p.spans.len());
    // The annotation entries themselves keep a position.
    assert!(p.spans.get("x-big").is_some() && p.spans.key("tracks[0].x_n").is_some());
}
