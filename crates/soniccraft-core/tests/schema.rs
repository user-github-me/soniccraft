//! JSON Schema tests: the golden file shipped with the Agent Skill, and agreement between the schema and
//! the compiler on every fixture, example and skill template.
//!
//! Regenerate the golden file with `SONICCRAFT_BLESS=1 cargo test -p soniccraft-core --test schema`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use soniccraft_core::schema::{SCHEMA_ID, timeline_schema, timeline_schema_pretty};
use soniccraft_core::{LowerOptions, SourceFormat, compile, diagnostics};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    crate_dir().join("../..")
}

fn golden_path() -> PathBuf {
    repo_root().join("skills/soniccraft/reference/timeline-schema.json")
}

#[test]
fn golden_schema_file_is_up_to_date() {
    let expected = timeline_schema_pretty();
    let path = golden_path();
    if std::env::var_os("SONICCRAFT_BLESS").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create reference dir");
        }
        std::fs::write(&path, &expected).expect("write golden schema");
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}); run with SONICCRAFT_BLESS=1 to create it",
            path.display()
        )
    });
    assert!(
        actual == expected,
        "{} is out of date; regenerate it with `SONICCRAFT_BLESS=1 cargo test -p soniccraft-core --test schema`",
        path.display()
    );
}

#[test]
fn schema_is_a_valid_draft_2020_12_schema() {
    let schema = timeline_schema();
    assert_eq!(schema["$id"], SCHEMA_ID);
    jsonschema::draft202012::new(&schema).expect("schema compiles");
    // The meta-schema check: the schema itself validates against draft 2020-12.
    assert!(
        jsonschema::meta::is_valid(&schema),
        "schema is not a valid JSON Schema document"
    );
}

/// Every timeline file the schema and the compiler must both accept: our fixtures, the repository examples
/// and the skill templates (the latter two may not exist yet).
fn valid_documents() -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(&crate_dir().join("tests/fixtures/valid"), &mut out);
    collect(&repo_root().join("examples"), &mut out);
    collect(
        &repo_root().join("skills/soniccraft/reference/templates"),
        &mut out,
    );
    out.sort();
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml" | "json")
        ) {
            // Only timelines: skip other JSON files (e.g. generated reports) that are not under fixtures.
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && !path.starts_with(crate_dir())
            {
                continue;
            }
            out.push(path);
        }
    }
}

fn load(path: &Path) -> (String, SourceFormat) {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let format = SourceFormat::from_extension(path.extension().and_then(|e| e.to_str()));
    (text, format)
}

#[test]
fn valid_documents_pass_schema_validation() {
    let schema = timeline_schema();
    let validator = jsonschema::draft202012::new(&schema).expect("schema compiles");
    let mut failures = Vec::new();
    let docs = valid_documents();
    assert!(!docs.is_empty(), "no fixtures found");
    for path in &docs {
        let (text, format) = load(path);
        let parsed = soniccraft_core::parse::parse_timeline(&text, format);
        let Some(value) = parsed.value else {
            failures.push(format!(
                "{}: does not parse: {:?}",
                path.display(),
                parsed.diagnostics
            ));
            continue;
        };
        for error in validator.iter_errors(&value) {
            failures.push(format!(
                "{}: {} at `{}`",
                path.display(),
                error,
                error.instance_path()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "schema rejects valid documents:\n{}",
        failures.join("\n")
    );
}

#[test]
fn valid_documents_compile_without_errors() {
    let mut failures = Vec::new();
    for path in valid_documents() {
        let (text, format) = load(&path);
        let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let compiled = compile(
            &text,
            format,
            &LowerOptions {
                base_dir,
                ..LowerOptions::default()
            },
        );
        for d in compiled.diagnostics.iter().filter(|d| d.is_error()) {
            failures.push(format!("{}: {d}", path.display()));
        }
        if !diagnostics::has_errors(&compiled.diagnostics) {
            assert!(
                compiled.program.is_some(),
                "{}: no program without errors",
                path.display()
            );
        }
    }
    assert!(
        failures.is_empty(),
        "valid documents do not compile:\n{}",
        failures.join("\n")
    );
}

#[test]
fn schema_rejects_common_mistakes() {
    let schema = timeline_schema();
    let validator = jsonschema::draft202012::new(&schema).expect("schema compiles");
    let base = json!({"version": "1.0", "tracks": [{"id": "v", "type": "speech", "sequence": [{"text": "hi"}]}]});
    assert!(validator.is_valid(&base));

    let mut cases: Vec<(&str, Value)> = Vec::new();
    let mut unknown = base.clone();
    unknown["tracks"][0]["fade_in"] = json!(100);
    cases.push(("unknown key", unknown));
    let mut both = base.clone();
    both["tracks"][0]["sequence"][0]["pause"] = json!(300);
    cases.push(("pause + cue", both));
    let mut anchor = base.clone();
    anchor["tracks"][0]["sequence"][0]["start_relative"] =
        json!({"after_cue": "a", "with_cue": "b"});
    cases.push(("two anchor targets", anchor));
    let mut end_before = json!({"version": "1.0", "tracks": [{"id": "m", "type": "music", "source": "a.wav",
        "end_relative": {"before_cue": "x"}}]});
    cases.push(("before_cue in end_relative", end_before.take()));
    cases.push((
        "music without source",
        json!({"version": "1.0", "tracks": [{"id": "m", "type": "music"}]}),
    ));
    cases.push((
        "speech without sequence",
        json!({"version": "1.0", "tracks": [{"id": "m", "type": "speech"}]}),
    ));
    cases.push((
        "missing version",
        json!({"tracks": [{"id": "m", "type": "sfx", "source": "a.wav"}]}),
    ));
    let mut volume = base.clone();
    volume["tracks"][0]["volume"] = json!(5);
    cases.push(("volume > 4", volume));
    let mut bad_id = base.clone();
    bad_id["tracks"][0]["id"] = json!("my track");
    cases.push(("id with space", bad_id));
    let mut format = base.clone();
    format["output"] = json!({"format": "aiff"});
    cases.push(("unknown format", format));

    for (name, doc) in cases {
        assert!(!validator.is_valid(&doc), "schema accepts {name}: {doc}");
    }

    // Annotations are accepted everywhere, including next to speaker names and on pauses.
    let annotated = json!({
        "version": "1.0", "x-generator": "agent",
        "speakers": {"narrator": null, "x-note": "free text"},
        "tracks": [{"id": "v", "type": "speech", "x_comment": [1, 2],
                    "sequence": [{"text": "hi", "x-take": 2}, {"pause": "300ms", "x-why": "breath"}]}]
    });
    let errors: Vec<String> = validator
        .iter_errors(&annotated)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn time_and_bitrate_patterns_accept_every_parser_form() {
    let schema = timeline_schema();
    let millis = jsonschema::draft202012::new(&schema["$defs"]["Millis"]).expect("Millis schema");
    let bitrate =
        jsonschema::draft202012::new(&schema["$defs"]["Bitrate"]).expect("Bitrate schema");
    for form in [
        "350ms",
        "350",
        "1.5s",
        "2m",
        "1m30s",
        "1m 30.5s",
        "01:30.5",
        "00:01:30.500",
        "-200ms",
        "+1s",
        "1h",
        "500 msecs",
        "2 hrs",
        "1.5 seconds",
        "3 minutes",
        "250 milliseconds",
        ".5s",
        "5.s",
        "1 sec",
        "2 mins",
    ] {
        assert!(
            soniccraft_core::values::parse_duration_str(form).is_ok(),
            "parser rejects {form:?}"
        );
        assert!(
            millis.is_valid(&json!(form)),
            "schema rejects duration {form:?}"
        );
    }
    for bad in ["", "ms", "1.2.3s", "abc", "1:", ":30", "5 5", "10ms 5"] {
        assert!(
            soniccraft_core::values::parse_duration_str(bad).is_err(),
            "parser accepts {bad:?}"
        );
        assert!(
            !millis.is_valid(&json!(bad)),
            "schema accepts duration {bad:?}"
        );
    }
    for form in [
        "320k", "320K", "320kbps", "320 kbps", "192000", "320KBPS", "96k bps",
    ] {
        let parsed = soniccraft_core::values::Bitrate(soniccraft_core::values::NumOrString::Str(
            form.into(),
        ))
        .to_bps();
        assert!(parsed.is_ok(), "parser rejects {form:?}");
        assert!(
            bitrate.is_valid(&json!(form)),
            "schema rejects bitrate {form:?}"
        );
    }
}
