//! Negative tests: every diagnostic is checked for its code, its exact path and an actionable hint, and
//! line/column information is checked where it matters (all through `soniccraft_core::compile`).

mod common;

use common::*;
use soniccraft_core::{LowerOptions, Severity};

// ---- unknown keys ------------------------------------------------------------------------------------

#[test]
fn unknown_top_level_key_suggests_the_right_key() {
    let c = yaml("version: \"1.0\"\ntrackz:\n  - id: a\n");
    let d = expect(&c, "unknown_field", "trackz", &["Did you mean `tracks`?"]);
    assert_eq!((d.line, d.column), (Some(2), Some(1)));
    // The resulting "no tracks" error is a consequence of the typo and is not reported separately.
    assert_eq!(count(&c, "empty_tracks"), 0, "{}", dump(&c));
}

#[test]
fn unknown_output_key_points_at_the_key() {
    let c = yaml("version: \"1.0\"\noutput:\n  format: mp3\n  formatt: wav\ntracks: []\n");
    let d = expect(
        &c,
        "unknown_field",
        "output.formatt",
        &["Did you mean `format`?"],
    );
    assert_eq!(d.message, "unknown field `formatt` in `output`");
    assert_eq!((d.line, d.column), (Some(4), Some(3)));
}

#[test]
fn camel_case_keys_suggest_snake_case() {
    let c = yaml("version: \"1.0\"\noutput:\n  sampleRate: 44100\ntracks: []\n");
    expect(
        &c,
        "unknown_field",
        "output.sampleRate",
        &["`sample_rate`", "snake_case"],
    );
}

#[test]
fn unknown_cue_key_uses_cue_candidates() {
    let src = BASE.replace(
        "        provider: mock\n",
        "        provider: mock\n        voice: alloy\n",
    );
    let c = yaml(&src);
    let d = expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[0].voice",
        &["Did you mean `voice_id`?"],
    );
    assert_eq!(d.message, "unknown field `voice` in cue `cue_01`");
    assert_eq!(d.line, Some(9));
}

#[test]
fn unknown_keys_in_nested_objects() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - text: "a"
        provider: mock
      - text: "b"
        provider: mock
        start_relative: {after: voice.cue1}
  - id: bed
    type: music
    source: bed.wav
    ducking:
      triggers: [voice]
"#,
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[1].start_relative.after",
        &["Did you mean `after_cue`?"],
    );
    let d = expect(
        &c,
        "unknown_field",
        "tracks[1].ducking.triggers",
        &["Did you mean `trigger_tracks`?"],
    );
    assert_eq!(
        d.message,
        "unknown field `triggers` in `ducking` of music track `bed`"
    );
    // The anchor without target and the ducking without triggers are consequences of the typos.
    assert_eq!(
        error_codes(&c),
        ["unknown_field", "unknown_field"],
        "{}",
        dump(&c)
    );
}

#[test]
fn misplaced_key_says_where_it_belongs() {
    let c = yaml(
        r#"version: "1.0"
sample_rate: 48000
tracks:
  - id: bed
    type: music
    source: bed.wav
    trigger_tracks: [voice]
    reduction_db: -12
"#,
    );
    expect(&c, "unknown_field", "sample_rate", &["belongs in `output`"]);
    expect(
        &c,
        "unknown_field",
        "tracks[0].trigger_tracks",
        &["belongs in `ducking`", "ducking: {trigger_tracks"],
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].reduction_db",
        &["belongs in `ducking`"],
    );
}

#[test]
fn unknown_key_inside_flag_or_object_value() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    normalize: {target: -18}
    trim_silence: {threshold: -45}
    sequence:
      - text: "a"
        provider: mock
"#,
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].normalize.target",
        &["Did you mean `target_lufs`?"],
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].trim_silence.threshold",
        &["Did you mean `threshold_db`?"],
    );
}

#[test]
fn unknown_key_without_suggestion_lists_valid_keys() {
    let c = yaml("version: \"1.0\"\nmix:\n  zebra: 1\ntracks: []\n");
    expect(
        &c,
        "unknown_field",
        "mix.zebra",
        &["Valid keys here: `reference_lufs`", "`x-`"],
    );
}

#[test]
fn unknown_track_key_mentions_track_type_applicability() {
    // `voice_ids` is close to `voice_id`, which is not valid on music tracks.
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: bed\n    type: music\n    source: a.wav\n    voice_ids: x\n",
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].voice_ids",
        &["`voice_id`", "does not apply to music tracks"],
    );
}

#[test]
fn annotations_are_ignored_everywhere() {
    let c = yaml(
        r#"version: "1.0"
x-generator: my-agent
x_notes: {anything: [1, 2, 3]}
speakers:
  x-comment: "not a speaker"
  narrator: {provider: mock, x-voice-notes: calm}
output: {format: wav, x-why: "editing"}
tracks:
  - id: voice
    type: speech
    x-color: blue
    sequence:
      - text: "Hello."
        speaker: narrator
        x-take: 3
      - pause: 300ms
        x-reason: breath
"#,
    );
    assert!(
        c.diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error && d.code != "unknown_field"),
        "{}",
        dump(&c)
    );
    let program = c.program.expect("program");
    assert_eq!(program.nodes.len(), 2);
}

// ---- types and element-by-element recovery ---------------------------------------------------------

#[test]
fn wrong_type_message_names_field_and_value() {
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: bed\n    type: music\n    source: a.wav\n    volume: loud\n",
    );
    let d = expect(&c, "invalid_type", "tracks[0].volume", &["plain number"]);
    assert_eq!(d.message, "`volume` must be a number, got string \"loud\"");
    assert_eq!((d.line, d.column), (Some(6), Some(13)));
}

#[test]
fn independent_type_errors_are_all_reported() {
    let c = yaml(
        r#"version: "1.0"
output:
  sample_rate: fast
tracks:
  - id: bed
    type: music
    source: a.wav
    pan: left
  - id: voice
    type: speech
    sequence:
      - text: "one"
        provider: mock
        gain_db: loud
      - "two"
      - text: "three"
        provider: mock
        start_relative: {after_cue: cue_x}
"#,
    );
    expect(&c, "invalid_type", "output.sample_rate", &["whole number"]);
    expect(&c, "invalid_type", "tracks[0].pan", &[]);
    expect(&c, "invalid_type", "tracks[1].sequence[0].gain_db", &[]);
    expect(
        &c,
        "invalid_type",
        "tracks[1].sequence[1]",
        &["- text: \"two\""],
    );
    // Elements after a broken one are still validated (with their original indices).
    expect(
        &c,
        "unknown_reference",
        "tracks[1].sequence[2].start_relative.after_cue",
        &["Known ids"],
    );
    // Dropped values do not cause follow-up errors (missing text for the broken item, etc.).
    assert_eq!(error_codes(&c).len(), 5, "{}", dump(&c));
}

#[test]
fn duration_type_errors_list_accepted_forms() {
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: bed\n    type: music\n    source: a.wav\n    fade_in_ms: [100]\n",
    );
    let d = expect(&c, "invalid_type", "tracks[0].fade_in_ms", &["\"1.5s\""]);
    assert!(
        d.message.contains("\"350ms\"") && d.message.contains("00:01:30.500"),
        "{}",
        d.message
    );
}

#[test]
fn bitrate_version_and_flag_object_forms() {
    let c = yaml(
        r#"version: [1]
output: {format: mp3, bitrate: true, export_subtitles: {vtt: true}}
tracks:
  - id: voice
    type: speech
    normalize: "yes"
    sequence:
      - text: "a"
        provider: mock
"#,
    );
    let v = expect(&c, "invalid_type", "version", &["version: \"1.0\""]);
    assert!(v.message.contains("\"1.0\""), "{}", v.message);
    let b = expect(&c, "invalid_type", "output.bitrate", &["\"192k\""]);
    assert!(b.message.contains("\"192k\""), "{}", b.message);
    let s = expect(
        &c,
        "invalid_type",
        "output.export_subtitles",
        &["[vtt, srt, json]"],
    );
    assert!(s.message.contains("a list of them"), "{}", s.message);
    let n = expect(
        &c,
        "invalid_type",
        "tracks[0].normalize",
        &["normalize: {target_lufs: -20}"],
    );
    assert!(
        n.message.contains("`true`, `false` or a mapping"),
        "{}",
        n.message
    );
    // A version that failed to parse is not reported as missing.
    assert_eq!(count(&c, "missing_field"), 0, "{}", dump(&c));
}

#[test]
fn numbers_for_string_fields_suggest_quoting() {
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: 7\n    type: speech\n    sequence:\n      - text: 42\n        provider: mock\n",
    );
    expect(
        &c,
        "invalid_type",
        "tracks[0].id",
        &["Quote the value: `id: \"7\"`"],
    );
    expect(
        &c,
        "invalid_type",
        "tracks[0].sequence[0].text",
        &["`text: \"42\"`"],
    );
    // No cascading "missing id" / "missing text" errors.
    assert_eq!(
        error_codes(&c),
        ["invalid_type", "invalid_type"],
        "{}",
        dump(&c)
    );
}

#[test]
fn yaml_1_1_booleans_stay_strings_and_get_a_hint() {
    let c = yaml("version: \"1.0\"\noutput:\n  normalize: yes\ntracks: []\n");
    expect(
        &c,
        "invalid_type",
        "output.normalize",
        &["`normalize: true`", "YAML 1.1"],
    );
}

// ---- syntax --------------------------------------------------------------------------------------------

#[test]
fn yaml_syntax_error_has_line_and_column() {
    let c = yaml("version: \"1.0\"\ntracks:\n  - id: a\n   type: speech\n");
    assert_eq!(error_codes(&c), ["yaml_syntax"]);
    let d = &c.diagnostics[0];
    assert_eq!(d.line, Some(4), "{d:?}");
    assert!(d.column.is_some());
    assert!(d.hint.as_deref().is_some_and(|h| h.contains("indentation")));
    assert!(c.program.is_none());
}

#[test]
fn yaml_duplicate_keys_are_syntax_errors() {
    let c = yaml("version: \"1.0\"\ntracks:\n  - id: a\n    id: b\n    type: speech\n");
    let d = &c.diagnostics[0];
    assert_eq!(d.code, "yaml_syntax");
    assert!(d.message.contains("duplicate key `id`"), "{}", d.message);
    assert_eq!(d.line, Some(4));
}

#[test]
fn multiple_yaml_documents_are_rejected() {
    let c = yaml(&format!("{BASE}---\n{BASE}"));
    let d = &c.diagnostics[0];
    assert_eq!(d.code, "yaml_syntax");
    assert!(d.message.contains("more than one YAML document"));
    assert!(
        d.hint
            .as_deref()
            .is_some_and(|h| h.contains("one timeline per file"))
    );
}

#[test]
fn non_mapping_documents_are_rejected() {
    let c = yaml("- version: 1\n");
    assert_eq!(error_codes(&c), ["invalid_type"]);
    assert!(c.diagnostics[0].message.contains("must be a mapping"));
    assert!(
        c.diagnostics[0]
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("version: \"1.0\""))
    );
}

#[test]
fn json_input_with_positions() {
    let src = r#"{
  "version": "1.0",
  "tracks": [
    {"id": "voice", "type": "speech", "fade_in_ms": 10,
     "sequence": [{"text": "Hi", "provider": "mock", "voice": "x"}]}
  ]
}"#;
    let c = json(src);
    let d = expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[0].voice",
        &["Did you mean `voice_id`?"],
    );
    assert_eq!(d.line, Some(5));
    let f = expect(
        &c,
        "field_not_applicable",
        "tracks[0].fade_in_ms",
        &["Fades apply to music/sfx clips"],
    );
    // Key position (the field itself is the problem).
    assert_eq!((f.line, f.column), (Some(4), Some(39)));
}

#[test]
fn json_syntax_and_duplicate_keys() {
    let c = json("{\n  \"version\": \"1.0\",\n  \"tracks\": [,]\n}");
    assert_eq!(error_codes(&c), ["json_syntax"]);
    assert_eq!(c.diagnostics[0].line, Some(3));

    let c = json("{\"version\": \"1.0\", \"tracks\": [], \"tracks\": []}");
    assert_eq!(error_codes(&c), ["json_syntax"]);
    assert!(c.diagnostics[0].message.contains("duplicate key `tracks`"));
}

// ---- secrets -----------------------------------------------------------------------------------------

#[test]
fn api_keys_in_provider_blocks_are_secrets() {
    let c = yaml(
        r#"version: "1.0"
providers:
  eleven:
    type: elevenlabs
    api_key: "abcdef0123456789abcdef0123456789"
tracks:
  - id: voice
    type: speech
    sequence:
      - text: "Hi"
        provider: eleven
        voice_id: "21m00Tcm4TlvDq8ikWAM"
"#,
    );
    let d = expect(
        &c,
        "secret_in_timeline",
        "providers.eleven.api_key",
        &[
            "api_key_env: ELEVENLABS_API_KEY",
            "export ELEVENLABS_API_KEY",
        ],
    );
    assert_eq!(d.line, Some(5));
    // Reported as a secret, not as an unknown field, and never echoed.
    assert_eq!(count(&c, "unknown_field"), 0, "{}", dump(&c));
    for d in &c.diagnostics {
        assert!(!d.message.contains("abcdef0123456789"), "{d}");
        assert!(
            !d.hint.as_deref().unwrap_or("").contains("abcdef0123456789"),
            "{d}"
        );
    }
    assert!(c.program.is_none());
}

#[test]
fn provider_key_values_are_secrets_and_redacted() {
    let key = "sk-proj-AbCdEf0123456789xyzXYZ0123";
    let c = yaml(&format!(
        "version: \"1.0\"\ntracks:\n  - id: voice\n    type: speech\n    sequence:\n      - text: \"Hi\"\n        provider: openai\n        voice_id: \"{key}\"\n        volume: \"{key}\"\n"
    ));
    expect(
        &c,
        "secret_in_timeline",
        "tracks[0].sequence[0].voice_id",
        &["api_key_env"],
    );
    expect(
        &c,
        "secret_in_timeline",
        "tracks[0].sequence[0].volume",
        &[],
    );
    let t = find(&c, "invalid_type", "tracks[0].sequence[0].volume");
    assert!(t.message.contains("looks like a secret"), "{}", t.message);
    for d in &c.diagnostics {
        assert!(!format!("{d}").contains(key), "secret leaked: {d}");
    }
}

#[test]
fn api_key_env_holding_the_key_itself() {
    let c = yaml(
        "version: \"1.0\"\nproviders:\n  openai: {api_key_env: \"sk-AbCdEf0123456789xyzXYZ0123\"}\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - {text: a, provider: openai}\n",
    );
    expect(
        &c,
        "secret_in_timeline",
        "providers.openai.api_key_env",
        &[
            "NAME of an environment variable",
            "api_key_env: OPENAI_API_KEY",
        ],
    );
    assert_eq!(count(&c, "invalid_value"), 0, "{}", dump(&c));
}

// ---- version -------------------------------------------------------------------------------------------

#[test]
fn version_errors() {
    let c = yaml(&BASE.replace("version: \"1.0\"\n", ""));
    expect(&c, "missing_field", "version", &["version: \"1.0\""]);

    let c = yaml(&BASE.replace("\"1.0\"", "\"2.0\""));
    let d = expect(&c, "unsupported_version", "version", &["version: \"1.0\""]);
    assert!(d.is_error());

    let c = yaml(&BASE.replace("\"1.0\"", "\"one\""));
    expect(&c, "invalid_value", "version", &["version: \"1.0\""]);

    // 1.x minor versions are accepted with a warning; "1", 1 and 1.0 are plain 1.0.
    let c = yaml(&BASE.replace("\"1.0\"", "\"1.3\""));
    assert_eq!(
        find(&c, "unsupported_version", "version").severity,
        Severity::Warning
    );
    for v in ["\"1\"", "1", "1.0"] {
        let c = yaml(&BASE.replace("\"1.0\"", v));
        assert_clean(&c);
        assert_eq!(c.program.unwrap().version, "1.0");
    }
}

// ---- enum values -----------------------------------------------------------------------------------

#[test]
fn bad_enum_values_suggest_names() {
    let c = yaml(
        r#"version: "1.0"
output: {format: flack, loudness_preset: podcats, export_subtitles: [vvt]}
timing: {speech_overlap: pushh}
tracks:
  - id: bed
    type: musik
    source: a.wav
    fade_curve: equalpower2
    ducking: {trigger_tracks: [bed2], mode: side_chain}
  - id: bed2
    type: sfx
    source: b.wav
"#,
    );
    expect(
        &c,
        "invalid_value",
        "output.format",
        &["Did you mean `flac`?"],
    );
    expect(
        &c,
        "invalid_value",
        "output.loudness_preset",
        &["Did you mean `podcast`?"],
    );
    expect(
        &c,
        "invalid_value",
        "output.export_subtitles[0]",
        &["Did you mean `vtt`?"],
    );
    expect(
        &c,
        "invalid_value",
        "timing.speech_overlap",
        &["Did you mean `push`?"],
    );
    expect(
        &c,
        "invalid_value",
        "tracks[0].type",
        &["Did you mean `music`?", "`speech`, `music`, `sfx`"],
    );
    expect(
        &c,
        "invalid_value",
        "tracks[0].fade_curve",
        &["Did you mean `equal_power`?"],
    );
    expect(
        &c,
        "invalid_value",
        "tracks[0].ducking.mode",
        &["Did you mean `sidechain`?"],
    );
    // An unknown format does not cascade into format-dependent warnings.
    assert_eq!(count(&c, "bitrate_ignored"), 0);
}

// ---- durations and ranges ----------------------------------------------------------------------------

#[test]
fn invalid_duration_strings() {
    let c = yaml(&BASE.replace(
        "        provider: mock\n",
        "        provider: mock\n        start_at_ms: \"1.5 parsecs\"\n",
    ));
    let d = expect(
        &c,
        "invalid_value",
        "tracks[0].sequence[0].start_at_ms",
        &["\"1.5s\"", "00:01:30.500"],
    );
    assert!(d.message.contains("1.5 parsecs"));
}

#[test]
fn negative_durations_except_offsets() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - text: "a"
        provider: mock
      - pause_duration: -200ms
      - text: "b"
        provider: mock
        start_relative: {with_cue: voice.cue1, offset_ms: -150}
  - id: bed
    type: music
    source: a.wav
    fade_out_ms: -1s
"#,
    );
    expect(
        &c,
        "invalid_value",
        "tracks[0].sequence[1].pause_duration",
        &["only allowed for `offset_ms`"],
    );
    expect(
        &c,
        "invalid_value",
        "tracks[1].fade_out_ms",
        &["only allowed for `offset_ms`"],
    );
    // offset_ms may be negative.
    assert!(
        c.diagnostics
            .iter()
            .all(|d| d.path.as_deref() != Some("tracks[0].sequence[2].start_relative.offset_ms"))
    );
    assert_eq!(error_codes(&c).len(), 2, "{}", dump(&c));
}

#[test]
fn value_ranges() {
    let c = yaml(
        r#"version: "1.0"
output: {target_lufs: -3, true_peak_db: 1}
mix: {reference_lufs: -80}
tracks:
  - id: bed
    type: music
    source: a.wav
    volume: 5
    pan: 1.5
    gain_db: 30
    ducking: {trigger_tracks: [voice], reduction_db: -80, threshold_db: 10}
  - id: voice
    type: speech
    volume: 3
    sequence:
      - text: "a"
        provider: mock
"#,
    );
    expect(&c, "invalid_value", "output.target_lufs", &["-16"]);
    expect(&c, "invalid_value", "output.true_peak_db", &["-1.0"]);
    expect(&c, "invalid_value", "mix.reference_lufs", &[]);
    expect(&c, "invalid_value", "tracks[0].volume", &["gain_db"]);
    expect(&c, "invalid_value", "tracks[0].pan", &["hard left"]);
    expect(
        &c,
        "invalid_value",
        "tracks[0].gain_db",
        &["-60 dB and +24 dB"],
    );
    expect(
        &c,
        "invalid_value",
        "tracks[0].ducking.reduction_db",
        &["reduction_db: -15"],
    );
    expect(&c, "invalid_value", "tracks[0].ducking.threshold_db", &[]);
    let loud = find(&c, "invalid_value", "tracks[1].volume");
    assert_eq!(loud.severity, Severity::Warning);
}

// ---- references ------------------------------------------------------------------------------------

#[test]
fn unknown_anchor_reference_with_did_you_mean() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - id: cue_01
        text: "a"
        provider: mock
      - id: cue_02
        text: "b"
        provider: mock
        start_relative: {after_cue: cue_1, offset_ms: 150}
"#,
    );
    let d = expect(
        &c,
        "unknown_reference",
        "tracks[0].sequence[1].start_relative.after_cue",
        &[
            "Did you mean `cue_01`?",
            "Known ids: `voice`, `cue_01`, `cue_02`",
        ],
    );
    assert_eq!(d.related, ["cue_01"]);
    assert_eq!((d.line, d.column), (Some(12), Some(37)));
}

#[test]
fn unknown_trigger_track_and_cue_as_trigger() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice_primary
    type: speech
    sequence:
      - {id: cue_01, text: "a", provider: mock}
  - id: bed
    type: music
    source: a.wav
    ducking:
      trigger_tracks: [voice_primry, cue_01]
"#,
    );
    expect(
        &c,
        "unknown_reference",
        "tracks[1].ducking.trigger_tracks[0]",
        &["Did you mean `voice_primary`?"],
    );
    expect(
        &c,
        "unknown_reference",
        "tracks[1].ducking.trigger_tracks[1]",
        &["use its track `voice_primary`"],
    );
}

#[test]
fn pauses_are_not_anchors() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {text: "a", provider: mock}
      - pause: 500
      - {text: "b", provider: mock, start_relative: {after_cue: voice.pause1}}
"#,
    );
    expect(
        &c,
        "unknown_reference",
        "tracks[0].sequence[2].start_relative.after_cue",
        &["pauses cannot be anchors"],
    );
}

#[test]
fn self_references() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: cue_01, text: "a", provider: mock, start_relative: {after_cue: cue_01}}
      - {id: cue_02, text: "b", provider: mock, start_relative: {with_cue: voice}}
  - id: bed
    type: music
    source: a.wav
    ducking: {trigger_tracks: [bed]}
"#,
    );
    expect(
        &c,
        "self_reference",
        "tracks[0].sequence[0].start_relative.after_cue",
        &["Anchor to a different"],
    );
    expect(
        &c,
        "self_reference",
        "tracks[0].sequence[1].start_relative.with_cue",
        &[],
    );
    expect(
        &c,
        "self_reference",
        "tracks[1].ducking.trigger_tracks[0]",
        &["Remove `bed`"],
    );
}

#[test]
fn dependency_cycle_is_reported_once_with_the_cycle() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: cue_01, text: "a", provider: mock}
      - {id: cue_02, text: "b", provider: mock, start_relative: {after_cue: cue_03}}
  - id: guest
    type: speech
    sequence:
      - {id: cue_03, text: "c", provider: mock, start_relative: {after_cue: cue_02}}
"#,
    );
    assert_eq!(count(&c, "dependency_cycle"), 1, "{}", dump(&c));
    let d = expect(
        &c,
        "dependency_cycle",
        "tracks[0].sequence[1].start_relative.after_cue",
        &["Break the cycle"],
    );
    assert_eq!(d.message, "dependency cycle: cue_02 → cue_03 → cue_02");
    assert_eq!(d.related, ["cue_02", "cue_03"]);
    assert!(c.program.is_none());
}

#[test]
fn dependency_cycle_through_sequential_items_and_track_starts() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: a
    type: speech
    start_relative: {after_cue: b}
    sequence:
      - {text: "a1", provider: mock}
  - id: b
    type: speech
    sequence:
      - {text: "b1", provider: mock, start_relative: {after_cue: a.cue1}}
      - {text: "b2", provider: mock}
"#,
    );
    let d = find(&c, "dependency_cycle", "tracks[0].start_relative.after_cue");
    assert!(
        d.message.contains("a.cue1") && d.message.contains("b.cue1") && d.message.contains(" → "),
        "{}",
        d.message
    );
    assert_eq!(count(&c, "dependency_cycle"), 1);
}

#[test]
fn anchoring_to_the_end_of_an_open_ended_loop() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: loop.wav
    loop: true
  - id: voice
    type: speech
    sequence:
      - {text: "a", provider: mock, start_relative: {after_cue: bed}}
      - {text: "b", provider: mock, start_relative: {with_cue: bed}}
"#,
    );
    let d = expect(
        &c,
        "anchor_open_ended",
        "tracks[1].sequence[0].start_relative.after_cue",
        &["with_cue: bed", "duration_ms"],
    );
    assert!(d.message.contains("plays until the end of the timeline"));
    // Anchoring to the start is fine.
    assert_eq!(count(&c, "anchor_open_ended"), 1);
}

// ---- conflicting fields --------------------------------------------------------------------------------

#[test]
fn start_at_and_start_relative_conflict() {
    let src = BASE.replace(
        "        provider: mock\n",
        "        provider: mock\n      - text: b\n        provider: mock\n        start_at_ms: 0\n        start_relative: {after_cue: cue_01}\n",
    );
    let c = yaml(&src);
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].sequence[1].start_relative",
        &["Keep one"],
    );
}

#[test]
fn anchors_need_exactly_one_target() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: cue_01, text: "a", provider: mock}
      - {text: "b", provider: mock, start_relative: {offset_ms: 100}}
      - {text: "c", provider: mock, start_relative: {after_cue: cue_01, with_cue: cue_01}}
"#,
    );
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].sequence[1].start_relative",
        &["start_relative: {after_cue"],
    );
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].sequence[2].start_relative.with_cue",
        &["exactly one"],
    );
}

#[test]
fn before_cue_is_invalid_in_end_relative() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: cue_01, text: "a", provider: mock}
  - id: bed
    type: music
    source: a.wav
    end_relative: {before_cue: cue_01}
"#,
    );
    expect(
        &c,
        "conflicting_fields",
        "tracks[1].end_relative.before_cue",
        &["`after_cue`", "`with_cue`"],
    );
}

#[test]
fn clip_length_conflicts() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    clips:
      - {source: a.wav, duration_ms: 1000, end_at_ms: 3000}
      - {source: b.wav, start_at_ms: 5000, end_at_ms: 2000}
"#,
    );
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].clips[0].end_at_ms",
        &["exactly one of `duration_ms`, `end_at_ms` or `end_relative`"],
    );
    expect(
        &c,
        "invalid_value",
        "tracks[0].clips[1].end_at_ms",
        &["later than `start_at_ms`"],
    );
}

#[test]
fn pause_and_cue_fields_in_one_item() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {text: "a", provider: mock, pause_duration: 300}
      - {pause: 300, speaker: narrator}
"#,
    );
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].sequence[0].pause_duration",
        &["split it into two items"],
    );
    expect(
        &c,
        "conflicting_fields",
        "tracks[0].sequence[1].speaker",
        &["remove `speaker`"],
    );
}

#[test]
fn shorthand_and_clips_conflict() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: a.wav
    loop: true
    clips:
      - source: b.wav
"#,
    );
    let d = expect(
        &c,
        "conflicting_fields",
        "tracks[0].source",
        &["Move `source`, `loop` into each entry of `clips`"],
    );
    assert!(
        d.message
            .contains("both a `clips` list and single-clip fields")
    );
}

// ---- field applicability ----------------------------------------------------------------------------

#[test]
fn audio_fields_on_speech_tracks() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    source: music.wav
    loop: true
    sequence:
      - {text: "a", provider: mock}
"#,
    );
    let d = expect(
        &c,
        "field_not_applicable",
        "tracks[0].source",
        &["`type: music` or `type: sfx`"],
    );
    assert_eq!(d.message, "`source` does not apply to speech track `voice`");
    assert_eq!((d.line, d.column), (Some(5), Some(5)));
    expect(&c, "field_not_applicable", "tracks[0].loop", &[]);
}

#[test]
fn speech_fields_on_audio_tracks() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: a.wav
    voice_id: alloy
    trim_silence: false
    sequence:
      - text: "nope"
"#,
    );
    let d = expect(
        &c,
        "field_not_applicable",
        "tracks[0].voice_id",
        &["only apply to speech tracks"],
    );
    assert_eq!(d.message, "`voice_id` does not apply to music track `bed`");
    expect(
        &c,
        "field_not_applicable",
        "tracks[0].trim_silence",
        &["trim_start_ms"],
    );
    expect(
        &c,
        "field_not_applicable",
        "tracks[0].sequence",
        &["`type: speech`"],
    );
}

// ---- ids -------------------------------------------------------------------------------------------------

#[test]
fn duplicate_ids_across_tracks_cues_and_clips() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: intro, text: "a", provider: mock}
      - {id: voice.cue3, text: "b", provider: mock}
      - {text: "c", provider: mock}
  - id: intro
    type: music
    clips:
      - {id: voice, source: a.wav}
"#,
    );
    let d = expect(
        &c,
        "duplicate_id",
        "tracks[1].id",
        &["share one id namespace"],
    );
    assert!(
        d.message
            .contains("already used by the cue at `tracks[0].sequence[0]`"),
        "{}",
        d.message
    );
    expect(&c, "duplicate_id", "tracks[1].clips[0].id", &[]);
    // The third cue's automatic id `voice.cue3` collides with the explicit id of the second cue.
    expect(
        &c,
        "duplicate_id",
        "tracks[0].sequence[2]",
        &["automatic id"],
    );
}

#[test]
fn invalid_ids() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: "my voice"
    type: speech
    sequence:
      - {id: "-intro", text: "a", provider: mock}
"#,
    );
    expect(&c, "invalid_id", "tracks[0].id", &["id: my_voice"]);
    expect(
        &c,
        "invalid_id",
        "tracks[0].sequence[0].id",
        &["must not start with"],
    );
}

#[test]
fn missing_track_id_and_type() {
    let c =
        yaml("version: \"1.0\"\ntracks:\n  - sequence:\n      - {text: \"a\", provider: mock}\n");
    expect(&c, "missing_field", "tracks[0].id", &["id: narration"]);
    expect(&c, "missing_field", "tracks[0].type", &["type: speech"]);
}

// ---- providers, voices, speakers ---------------------------------------------------------------------

#[test]
fn missing_provider_is_grouped_per_track() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - text: "a"
      - text: "b"
      - text: "c"
  - id: other
    type: speech
    sequence:
      - text: "d"
"#,
    );
    let d = expect(
        &c,
        "missing_provider",
        "tracks[0]",
        &["defaults: {provider: ...}", "`mock` works offline"],
    );
    assert!(
        d.message.starts_with("3 cues of track `voice`"),
        "{}",
        d.message
    );
    assert_eq!(d.related, ["voice.cue1", "voice.cue2", "voice.cue3"]);
    expect(&c, "missing_provider", "tracks[1].sequence[0]", &[]);
    assert_eq!(count(&c, "missing_provider"), 2);
}

#[test]
fn missing_and_default_voices() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: a, text: "a", provider: elevenlabs}
      - {id: b, text: "b", provider: openai}
"#,
    );
    let d = expect(
        &c,
        "missing_voice",
        "tracks[0].sequence[0]",
        &["soniccraft voices elevenlabs"],
    );
    assert!(
        d.message
            .contains("provider `elevenlabs` has no default voice")
    );
    let info = expect(&c, "default_voice", "tracks[0].sequence[1]", &["voice_id"]);
    assert_eq!(info.severity, Severity::Info);
    assert!(info.message.contains("`alloy`"));
}

#[test]
fn unknown_provider_suggests_configured_and_builtin_names() {
    let c = yaml(
        r#"version: "1.0"
providers:
  studio_eleven: {type: elevenlabs}
defaults: {provider: mok}
tracks:
  - id: voice
    type: speech
    sequence:
      - {text: "a", provider: studio_elevn, voice_id: x}
      - {text: "b", provider: polly}
"#,
    );
    expect(
        &c,
        "unknown_provider",
        "defaults.provider",
        &["Did you mean `mock`?"],
    );
    expect(
        &c,
        "unknown_provider",
        "tracks[0].sequence[0].provider",
        &["Did you mean `studio_eleven`?"],
    );
    expect(
        &c,
        "unknown_provider",
        "tracks[0].sequence[1].provider",
        &["`studio_eleven`", "`mock`"],
    );
    assert_eq!(count(&c, "unknown_provider"), 3, "{}", dump(&c));
}

#[test]
fn custom_provider_names_need_a_type() {
    let c = yaml(
        r#"version: "1.0"
providers:
  studio: {api_key_env: STUDIO_KEY}
  local: {type: kokoroo}
tracks:
  - id: voice
    type: speech
    sequence:
      - {text: "a", provider: studio}
"#,
    );
    expect(
        &c,
        "missing_field",
        "providers.studio.type",
        &["Add `type:`"],
    );
    expect(
        &c,
        "invalid_value",
        "providers.local.type",
        &["Did you mean `kokoro`?"],
    );
    // Cues using a broken instance get no extra provider errors.
    assert_eq!(count(&c, "unknown_provider"), 0);
}

#[test]
fn misspelled_speakers_are_errors() {
    let c = yaml(
        r#"version: "1.0"
speakers:
  narrator: {provider: mock}
tracks:
  - id: voice
    type: speech
    sequence:
      - {text: "a", speaker: narator}
"#,
    );
    let d = expect(
        &c,
        "unknown_speaker",
        "tracks[0].sequence[0].speaker",
        &["Did you mean `narrator`?"],
    );
    // A close match to a defined speaker is a typo (an error); the voice is resolved as if the hint were
    // applied, so the provider that `narrator` sets is not reported missing.
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(error_codes(&c), ["unknown_speaker"], "{}", dump(&c));
}

// ---- empty things ------------------------------------------------------------------------------------

#[test]
fn empty_tracks_sequence_and_text() {
    let c = yaml("version: \"1.0\"\ntracks: []\n");
    expect(&c, "empty_tracks", "tracks", &["Add at least one track"]);

    let c = yaml("version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence: []\n");
    expect(
        &c,
        "empty_sequence",
        "tracks[0].sequence",
        &["Add at least one cue"],
    );

    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - text: \"   \"\n        provider: mock\n",
    );
    expect(
        &c,
        "empty_text",
        "tracks[0].sequence[0].text",
        &["Write the words"],
    );

    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - pause: 1s\n",
    );
    assert_eq!(
        find(&c, "empty_sequence", "tracks[0].sequence").severity,
        Severity::Warning
    );
}

#[test]
fn music_track_without_source() {
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: bed\n    type: music\n    loop: true\n    clips: []\n",
    );
    expect(&c, "empty_sequence", "tracks[0].clips", &["- source:"]);
    let c = yaml("version: \"1.0\"\ntracks:\n  - id: bed\n    type: sfx\n    volume: 0.5\n");
    expect(
        &c,
        "missing_field",
        "tracks[0].source",
        &["source: path/to/audio.wav"],
    );
}

#[test]
fn first_clip_crossfade_is_a_warning() {
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: bed\n    type: music\n    clips:\n      - {source: a.wav, crossfade_ms: 500}\n      - {source: b.wav, crossfade_ms: 500}\n",
    );
    let d = expect(
        &c,
        "field_not_applicable",
        "tracks[0].clips[0].crossfade_ms",
        &["fade_in_ms"],
    );
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(count(&c, "field_not_applicable"), 1);
    let program = c.program.expect("warnings only");
    assert_eq!(program.nodes[0].as_clip().unwrap().crossfade_ms, 0.0);
    assert_eq!(program.nodes[1].as_clip().unwrap().crossfade_ms, 500.0);
}

// ---- output ----------------------------------------------------------------------------------------------

#[test]
fn output_format_checks() {
    let c = yaml(&BASE.replace(
        "tracks:",
        "output: {format: mp3, sample_rate: 96000, bitrate: 300k, path: out.wav}\ntracks:",
    ));
    expect(
        &c,
        "unsupported_sample_rate",
        "output.sample_rate",
        &["sample_rate: 48000"],
    );
    let adj = expect(
        &c,
        "bitrate_adjusted",
        "output.bitrate",
        &["bitrate: \"320k\""],
    );
    assert_eq!(adj.severity, Severity::Info);
    expect(
        &c,
        "format_extension_mismatch",
        "output.format",
        &["out.mp3", "format: wav"],
    );

    let c = yaml(&BASE.replace(
        "tracks:",
        "output: {format: flac, bit_depth: 32, bitrate: 192k}\ntracks:",
    ));
    expect(
        &c,
        "unsupported_bit_depth",
        "output.bit_depth",
        &["bit_depth: 24"],
    );
    expect(
        &c,
        "bitrate_ignored",
        "output.bitrate",
        &["Remove `bitrate`"],
    );

    let c = yaml(&BASE.replace("tracks:", "output: {path: out.aiff}\ntracks:"));
    expect(&c, "invalid_value", "output.path", &[".wav, .mp3, .flac"]);
}

#[test]
fn output_override_mismatch_has_no_document_path_for_the_override() {
    let options = LowerOptions {
        output_path_override: Some("render.wav".into()),
        ..LowerOptions::default()
    };
    let c = yaml_with(
        &BASE.replace("tracks:", "output: {format: mp3}\ntracks:"),
        &options,
    );
    let d = expect(
        &c,
        "format_extension_mismatch",
        "output.format",
        &["render.mp3"],
    );
    assert!(d.message.contains("render.wav"));
    // The path came from the render call, so the hint also offers the call's own format option.
    let hint = d.hint.as_deref().unwrap_or_default();
    assert!(
        hint.contains("--format wav") && hint.contains(r#"format: "wav""#),
        "{hint}"
    );
}

#[test]
fn diagnostics_are_sorted_by_position() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: a.wav
    volume: -1
  - id: voice
    type: speech
    sequence:
      - {text: "a", provider: mok}
output: {format: wavv}
"#,
    );
    let lines: Vec<u32> = c
        .diagnostics
        .iter()
        .map(|d| d.line.expect("every diagnostic is positioned"))
        .collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted, "{}", dump(&c));
    assert_eq!(c.diagnostics.len(), 3, "{}", dump(&c));
}

// ---- blanket properties ------------------------------------------------------------------------------

#[test]
fn every_diagnostic_is_positioned_and_every_error_has_a_hint() {
    let dir = fixture("invalid");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("invalid fixtures")
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    assert!(files.len() >= 3);
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        let format = soniccraft_core::SourceFormat::from_extension(
            path.extension().and_then(|e| e.to_str()),
        );
        let c = soniccraft_core::compile(&text, format, &LowerOptions::default());
        let name = path.display();
        assert!(c.program.is_none(), "{name} should not compile");
        assert!(
            error_codes(&c).len() >= 8,
            "{name}: expected many errors:\n{}",
            dump(&c)
        );
        let mut seen = std::collections::HashSet::new();
        for d in &c.diagnostics {
            assert!(d.path.is_some(), "{name}: diagnostic without path: {d}");
            assert!(
                d.line.is_some() && d.column.is_some(),
                "{name}: diagnostic without position: {d}"
            );
            if d.is_error() {
                assert!(
                    d.hint.as_deref().is_some_and(|h| h.len() > 10),
                    "{name}: error without hint: {d}"
                );
            }
            assert!(
                seen.insert((d.code.clone(), d.path.clone())),
                "{name}: duplicate diagnostic {} at {:?}:\n{}",
                d.code,
                d.path,
                dump(&c)
            );
        }
    }
}
