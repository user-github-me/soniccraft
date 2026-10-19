//! Value checks added in the hardening pass: the master loudness range, portable paths in messages, ids
//! with surrounding whitespace, seconds written as milliseconds, misspelled pauses, suppressed cascades
//! and the asset list of `Compiled`.

mod common;

use std::path::PathBuf;

use common::*;
use soniccraft_core::{AssetRef, LowerOptions, Severity};

// ---- output.target_lufs -------------------------------------------------------------------------------

#[test]
fn master_loudness_targets_stay_above_the_measurement_gate() {
    let with = |target: &str| {
        yaml(&BASE.replace(
            "tracks:",
            &format!("output: {{target_lufs: {target}}}\ntracks:"),
        ))
    };
    let c = with("-70");
    let d = expect(
        &c,
        "invalid_value",
        "output.target_lufs",
        &["between -60 and -5 LUFS", "absolute gate"],
    );
    assert_eq!(
        d.message,
        "`target_lufs` must be between -60 and -5 (got -70)"
    );
    assert!(c.program.is_none());
    expect(
        &with("16"),
        "invalid_value",
        "output.target_lufs",
        &["use `target_lufs: -16`"],
    );
    let p = with("-60").program.expect("-60 is allowed");
    assert_eq!(p.output.target_lufs, -60.0);
    // Source-normalization targets keep their range.
    assert!(
        yaml(&BASE.replace("tracks:", "mix: {reference_lufs: -65}\ntracks:"))
            .program
            .is_some()
    );
}

// ---- no absolute paths in messages ----------------------------------------------------------------------

#[test]
fn format_mismatch_never_prints_an_absolute_path() {
    let base = BASE.replace("tracks:", "output: {format: wav}\ntracks:");
    for (override_path, shown, renamed) in [
        ("/private/tmp/a/out/demo.mp3", "`demo.mp3`", "`demo.wav`"),
        ("C:\\renders\\demo.mp3", "`demo.mp3`", "`demo.wav`"),
        ("out/demo.mp3", "`out/demo.mp3`", "`out/demo.wav`"),
    ] {
        let options = LowerOptions {
            output_path_override: Some(override_path.into()),
            ..LowerOptions::default()
        };
        let c = yaml_with(&base, &options);
        let d = expect(
            &c,
            "format_extension_mismatch",
            "output.format",
            &[renamed, "format: mp3"],
        );
        assert!(d.message.contains(shown), "{}", d.message);
        for text in [&d.message, d.hint.as_ref().expect("hint")] {
            assert!(
                !text.contains("/private") && !text.contains("renders"),
                "{text}"
            );
        }
        // The path itself is kept for rendering.
        assert_eq!(
            c.program.expect("warning only").output.path.as_deref(),
            Some(override_path)
        );
    }
}

#[test]
fn diagnostics_do_not_depend_on_the_render_directory() {
    let base = BASE.replace("tracks:", "output: {format: wav}\ntracks:");
    let compile_in = |dir: &str| {
        let options = LowerOptions {
            base_dir: PathBuf::from(dir),
            output_path_override: Some(format!("{dir}/out/demo.mp3")),
            ..LowerOptions::default()
        };
        yaml_with(&base, &options).diagnostics
    };
    assert_eq!(compile_in("/tmp/render-a"), compile_in("/tmp/render-b"));
}

// ---- ids ------------------------------------------------------------------------------------------------

#[test]
fn ids_with_surrounding_whitespace_are_invalid() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: " v "
    type: speech
    sequence:
      - {id: " c1", text: "a", provider: mock}
      - {text: "b", provider: mock, start_relative: {after_cue: c1}}
  - id: m
    type: music
    clips:
      - {id: "c2\t", source: a.wav, start_relative: {with_cue: v}}
"#,
    );
    let d = expect(&c, "invalid_id", "tracks[0].id", &["`id: v`"]);
    assert_eq!(d.message, "track id \" v \" has whitespace around it");
    expect(&c, "invalid_id", "tracks[0].sequence[0].id", &["`id: c1`"]);
    expect(&c, "invalid_id", "tracks[1].clips[0].id", &["`id: c2`"]);
    // References to the trimmed ids still resolve: no cascading unknown_reference.
    assert_eq!(
        error_codes(&c),
        ["invalid_id", "invalid_id", "invalid_id"],
        "{}",
        dump(&c)
    );
    assert!(c.program.is_none());
}

// ---- seconds written as milliseconds --------------------------------------------------------------------

#[test]
fn small_bare_numbers_that_look_like_seconds_are_warned() {
    // The reviewer's c02-units.yaml: a 30 ms bed with 2 and 3 ms fades, a 2.5 ms track start, a 1.5 ms pause.
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: assets/audio/synthwave_loop.wav
    duration_ms: 30
    fade_in_ms: 2
    fade_out_ms: 3
  - id: narration
    type: speech
    provider: mock
    start_at_ms: 2.5
    sequence:
      - text: "Hello."
      - pause: 1.5
      - text: "World."
"#,
    );
    let expected = [
        (
            "tracks[0].duration_ms",
            "`duration_ms: 30` is 30 milliseconds",
            "`duration_ms: \"30s\"`",
        ),
        (
            "tracks[0].fade_in_ms",
            "`fade_in_ms: 2` is 2 milliseconds",
            "`fade_in_ms: \"2s\"`",
        ),
        (
            "tracks[0].fade_out_ms",
            "`fade_out_ms: 3` is 3 milliseconds",
            "`fade_out_ms: \"3s\"`",
        ),
        (
            "tracks[1].start_at_ms",
            "`start_at_ms: 2.5` is 2.5 milliseconds",
            "`start_at_ms: \"2.5s\"`",
        ),
        (
            "tracks[1].sequence[1].pause",
            "`pause: 1.5` is 1.5 milliseconds",
            "`pause: \"1.5s\"`",
        ),
    ];
    for (path, message, hint) in expected {
        let d = expect(&c, "suspicious_value", path, &[hint, "if you meant"]);
        assert_eq!(d.message, message);
        assert_eq!(d.severity, Severity::Warning);
    }
    assert_eq!(
        count(&c, "suspicious_value"),
        expected.len(),
        "{}",
        dump(&c)
    );
    assert!(c.program.is_some(), "warnings only");
}

#[test]
fn seconds_heuristics_per_field() {
    let c = yaml(
        r#"version: "1.0"
output: {duration_ms: 45}
tracks:
  - id: v
    type: speech
    provider: mock
    sequence:
      - {id: a, text: "a", start_at_ms: 0}
      - {id: b, text: "b", start_at_ms: 2500}
      - {id: c, text: "c", start_relative: {after_cue: a, offset_ms: -1.5}}
      - {id: d, text: "d", start_relative: {after_cue: a, offset_ms: 2}}
      - pause_duration: 0
      - pause: 350
      - pause: "1.5s"
  - id: m
    type: music
    clips:
      - {id: c1, source: a.wav, duration_ms: "30", crossfade_ms: 5, fade_in_ms: "2ms"}
      - {id: c2, source: b.wav, crossfade_ms: 5, end_at_ms: 60}
      - {id: c3, source: c.wav, start_at_ms: 10, end_at_ms: 60, fade_out_ms: 10}
      - {id: c4, source: d.wav, duration_ms: 100, fade_in_ms: 0}
"#,
    );
    let warned: Vec<&str> = c
        .diagnostics
        .iter()
        .filter(|d| d.code == "suspicious_value")
        .filter_map(|d| d.path.as_deref())
        .collect();
    assert_eq!(
        warned,
        [
            "output.duration_ms",
            "tracks[0].sequence[2].start_relative.offset_ms",
            "tracks[1].clips[0].duration_ms",
            "tracks[1].clips[1].crossfade_ms",
            "tracks[1].clips[1].end_at_ms",
            "tracks[1].clips[2].end_at_ms",
        ],
        "{}",
        dump(&c)
    );
    // A bare string number is a bare number; the first clip's crossfade is ignored (and reported) instead.
    expect(
        &c,
        "field_not_applicable",
        "tracks[1].clips[0].crossfade_ms",
        &[],
    );
    // An end time with a bare start converts both.
    expect(
        &c,
        "suspicious_value",
        "tracks[1].clips[2].end_at_ms",
        &["`start_at_ms: \"10s\"` and `end_at_ms: \"60s\"`"],
    );
    expect(
        &c,
        "suspicious_value",
        "tracks[1].clips[1].end_at_ms",
        &["`end_at_ms: \"60s\"` if you meant 60 seconds"],
    );
}

#[test]
fn strings_with_units_never_look_like_seconds() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: a.wav
    start_at_ms: "2.5ms"
    duration_ms: "30ms"
    fade_in_ms: "2ms"
    fade_out_ms: "00:00.005"
"#,
    );
    assert_eq!(count(&c, "suspicious_value"), 0, "{}", dump(&c));
}

// ---- misspelled pauses ----------------------------------------------------------------------------------

#[test]
fn a_misspelled_pause_is_not_also_a_cue_without_text() {
    for item in [
        "{pause_ms: 300}",
        "{silence: 300}",
        "{wait: 1s}",
        "{delay_ms: 200}",
        "{Pause: 300}",
        "{pause_ms: 300, x-note: breath}",
    ] {
        let c = yaml(&format!(
            "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - {{text: hi, provider: mock}}\n      - {item}\n"
        ));
        assert_eq!(count(&c, "missing_field"), 0, "{item}:\n{}", dump(&c));
        assert_eq!(error_codes(&c), ["unknown_field"], "{item}:\n{}", dump(&c));
    }
    // The item is lowered as a pause: no voice diagnostics for it, and the next cue keeps the automatic id it
    // has once the key is fixed (`v.cue2`, not `v.cue3`).
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - {text: hi, provider: mock}\n      - {wait: 300}\n      - {text: there, provider: mock}\n",
    );
    assert_eq!(error_codes(&c), ["unknown_field"], "{}", dump(&c));
    let voice = find(&c, "default_voice", "tracks[0]");
    assert_eq!(voice.related, ["v.cue1", "v.cue2"], "{}", dump(&c));
    // An item with a real cue field still needs its text.
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - {pause_ms: 300, speaker: a, provider: mock}\n",
    );
    expect(&c, "missing_field", "tracks[0].sequence[0].text", &["text"]);
}

// ---- cascades -------------------------------------------------------------------------------------------

#[test]
fn a_dropped_anchor_target_is_not_also_a_missing_target() {
    // The reviewer's agenty.yaml: `after_cue: [cue_01]` is an invalid_type; "needs exactly one of …" was a
    // contradictory second error.
    let c = yaml(
        r#"version: "1.0"
defaults: {provider: mock, voice_id: n}
tracks:
  - id: v
    type: speech
    sequence:
      - id: cue_01
        text: "one"
      - text: "four"
        start_relative: {after_cue: [cue_01]}
"#,
    );
    assert_eq!(error_codes(&c), ["invalid_type"], "{}", dump(&c));
    // Without any target the error stays.
    let c = yaml(&BASE.replace(
        "provider: mock\n",
        "provider: mock\n      - {text: b, provider: mock, start_relative: {offset_ms: 100}}\n",
    ));
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].sequence[1].start_relative",
        &["after_cue: cue_01"],
    );
}

// ---- assets ---------------------------------------------------------------------------------------------

#[test]
fn assets_are_listed_even_when_the_document_has_errors() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: " assets/bed.wav "
    loop: true
    volumee: 2
  - id: voice
    type: speech
    source: ignored.wav
    sequence:
      - {txt: "missing text"}
  - id: fx
    type: sfx
    clips:
      - {source: assets/hit.wav}
      - {id: whoosh, source: assets/whoosh.wav, start_at_ms: "1:2:3:4"}
      - {source: ""}
      - {source: 5}
  - type: sfx
    source: untyped-id.wav
"#,
    );
    assert!(c.program.is_none());
    let got: Vec<(&str, &str, &str)> = c
        .assets
        .iter()
        .map(|a| (a.source.as_str(), a.path.as_str(), a.id.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("assets/bed.wav", "tracks[0].source", "bed"),
            ("assets/hit.wav", "tracks[2].clips[0].source", "fx.clip1"),
            ("assets/whoosh.wav", "tracks[2].clips[1].source", "whoosh"),
            ("untyped-id.wav", "tracks[3].source", "track4"),
        ]
    );
    assert_eq!(
        c.assets[0],
        AssetRef {
            source: "assets/bed.wav".into(),
            path: "tracks[0].source".into(),
            id: "bed".into(),
            line: Some(5),
            column: Some(13),
        }
    );
    // A document that does not parse has no assets.
    assert!(yaml("version: [").assets.is_empty());
}

#[test]
fn assets_of_valid_timelines_match_the_program_clips() {
    let c = yaml(&read_fixture("valid/readme.yaml"));
    let p = c.program.as_ref().expect("program");
    let from_program: Vec<(&str, String)> = p
        .clips()
        .map(|(_, node, clip)| (clip.source.as_str(), format!("{}.source", node.path)))
        .collect();
    let from_assets: Vec<(&str, String)> = c
        .assets
        .iter()
        .map(|a| (a.source.as_str(), a.path.clone()))
        .collect();
    assert_eq!(from_assets, from_program);
}
