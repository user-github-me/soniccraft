//! Hints an agent can apply mechanically (DESIGN.md §0.2): `#` comments that cut cue text short, synonyms of
//! DSL keys, millisecond units, converted values, exact enum spellings and the time grammar. Each test is a
//! reviewer's repro, checked end to end through `soniccraft_core::compile`.

mod common;

use common::*;
use soniccraft_core::Severity;

// ---- comment_in_text --------------------------------------------------------------------------------

const HASH: &str = r#"version: "1.0"
output: { path: out/hash.wav, export_subtitles: [srt] }
tracks:
  - id: narration
    type: speech
    provider: mock
    sequence:
      - id: tip
        text: Tip #1: always duck the music bed.
      - id: rule
        text: Remember rule #2 of audio, never clip the master.
"#;

#[test]
fn an_unquoted_hash_in_cue_text_is_a_warning() {
    let c = yaml(HASH);
    let d = expect(
        &c,
        "comment_in_text",
        "tracks[0].sequence[0].text",
        &["Quote the text: text: \"Tip #1: always duck the music bed.\""],
    );
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "the text of cue `tip` stops at ` #`: YAML read `#1: always duck the music bed.` as a comment"
    );
    // The position of the `#`.
    assert_eq!((d.line, d.column), (Some(9), Some(19)));
    expect(
        &c,
        "comment_in_text",
        "tracks[0].sequence[1].text",
        &["text: \"Remember rule #2 of audio, never clip the master.\""],
    );
    // Warnings only: the timeline still compiles (with the text YAML read).
    let program = c.program.expect("program");
    assert_eq!(
        program.nodes[0].as_cue().map(|c| c.text.as_str()),
        Some("Tip")
    );
}

#[test]
fn quoted_text_captions_and_own_line_comments_are_fine() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: v
    type: speech
    provider: mock
    sequence:
      # a comment on its own line
      - text: "Tip #1: quoted" # a real comment
      - {text: flow, caption: Shown}
      - text: >
          Folded #1 text
      - text: Spoken
        caption: Caption #3 here
"#,
    );
    assert_eq!(count(&c, "comment_in_text"), 1, "{}", dump(&c));
    let d = expect(
        &c,
        "comment_in_text",
        "tracks[0].sequence[3].caption",
        &["caption: \"Caption #3 here\""],
    );
    assert!(
        d.message.starts_with("the caption of cue `v.cue4`"),
        "{}",
        d.message
    );
}

#[test]
fn comment_hints_never_echo_secrets() {
    let c = yaml(
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    provider: mock\n    sequence:\n      - text: Key # sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123\n",
    );
    let d = find(&c, "comment_in_text", "tracks[0].sequence[0].text");
    assert!(!format!("{d}").contains("AbCdEf"), "{d}");
}

// ---- unknown keys: units, synonyms, locations ---------------------------------------------------------

#[test]
fn time_keys_without_unit_mention_milliseconds() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: bed.wav
    start: 0
    duration: 30
  - id: narration
    type: speech
    provider: mock
    start_at: 2.5
    sequence:
      - text: "Hello."
        start_ms: 2500
      - pause_s: 1.5
"#,
    );
    let d = expect(&c, "unknown_field", "tracks[0].duration", &[]);
    assert_eq!(
        d.hint.as_deref(),
        Some(
            "Did you mean `duration_ms`? Times are milliseconds: write `duration_ms: \"30s\"` (30000 ms) if you \
             meant seconds"
        )
    );
    expect(
        &c,
        "unknown_field",
        "tracks[1].start_at",
        &[
            "`start_at_ms`",
            "`start_at_ms: \"2.5s\"` (2500 ms) if you meant seconds",
        ],
    );
    // 0 is the same in both units; a key that says `ms` keeps its number; one that says seconds converts.
    assert_eq!(
        find(&c, "unknown_field", "tracks[0].start").hint.as_deref(),
        Some("Did you mean `start_at_ms`? Write `start_at_ms: 0`")
    );
    assert_eq!(
        find(&c, "unknown_field", "tracks[1].sequence[0].start_ms")
            .hint
            .as_deref(),
        Some("Did you mean `start_at_ms`? Write `start_at_ms: 2500`")
    );
    expect(
        &c,
        "unknown_field",
        "tracks[1].sequence[1].pause_s",
        &["Write `pause: \"1.5s\"`"],
    );
}

#[test]
fn synonyms_of_source_point_to_source() {
    for key in [
        "file", "path", "src", "audio", "url", "filename", "sound", "sample",
    ] {
        let c = yaml(&format!(
            "version: \"1.0\"\ntracks:\n  - id: m\n    type: music\n    {key}: assets/audio/synthwave_loop.wav\n"
        ));
        let d = expect(&c, "unknown_field", &format!("tracks[0].{key}"), &[]);
        assert_eq!(
            d.hint.as_deref(),
            Some("Did you mean `source`? Write `source: \"assets/audio/synthwave_loop.wav\"`"),
            "{key}"
        );
        // The missing `source` is the same mistake.
        assert_eq!(error_codes(&c), ["unknown_field"], "{key}: {}", dump(&c));
    }
}

#[test]
fn synonyms_of_voice_id_and_anchors() {
    for key in [
        "voice",
        "voice_name",
        "tts_voice",
        "speaker_voice",
        "narrator_voice",
    ] {
        let c = yaml(&format!(
            "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    provider: mock\n    {key}: x\n    sequence: [ {{ text: hi }} ]\n"
        ));
        assert_eq!(
            find(&c, "unknown_field", &format!("tracks[0].{key}"))
                .hint
                .as_deref(),
            Some("Did you mean `voice_id`? Write `voice_id: x`"),
            "{key}"
        );
    }
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: host
    type: speech
    provider: mock
    sequence:
      - {id: q1, text: "What is it?"}
      - {id: a2, text: "An audio compiler.", follows: q1}
      - {text: "Together.", start_with: a2}
  - id: hit
    type: sfx
    source: hit.wav
    start_after: a2
"#,
    );
    assert_eq!(
        find(&c, "unknown_field", "tracks[1].start_after")
            .hint
            .as_deref(),
        Some("To start after `a2` ends, write `start_relative: {after_cue: a2}`")
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[1].follows",
        &["`start_relative: {after_cue: q1}`"],
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[2].start_with",
        &["`start_relative: {with_cue: a2}`"],
    );
}

#[test]
fn a_time_written_as_an_id_points_to_the_anchor() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: guest
    type: speech
    provider: mock
    sequence:
      - {id: a2, text: "It ducks music too."}
  - id: hit
    type: sfx
    source: impact.wav
    start_at_ms: a2
"#,
    );
    expect(
        &c,
        "invalid_value",
        "tracks[1].start_at_ms",
        &["`start_relative: {after_cue: a2}`"],
    );
    assert_eq!(error_codes(&c), ["invalid_value"], "{}", dump(&c));
}

#[test]
fn label_belongs_in_a_speakers_entry() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: v
    type: speech
    provider: mock
    label: Narrator
    sequence:
      - {id: c1, text: "Hello.", label: Bob}
"#,
    );
    for path in ["tracks[0].label", "tracks[0].sequence[0].label"] {
        let d = find(&c, "unknown_field", path);
        assert_eq!(
            d.hint.as_deref(),
            Some(
                "`label` is not valid here; it belongs in a `speakers` entry (label is the subtitle name of a \
                 speaker)"
            ),
            "{path}"
        );
    }
}

#[test]
fn timing_typos_name_local_keys_and_pauses() {
    let c = yaml(
        r#"version: "1.0"
defaults: {provider: mock, voice_id: v}
timing: {overlap: push}
tracks:
  - id: A
    type: speech
    sequence:
      - {id: x, text: "a"}
      - {pause_ms: 300}
      - {silence: 300}
      - {wait: 1s}
"#,
    );
    assert_eq!(
        find(&c, "unknown_field", "timing.overlap").hint.as_deref(),
        Some("Did you mean `speech_overlap`? Write `speech_overlap: push`")
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[1].pause_ms",
        &["Did you mean `pause`? Write `pause: 300`"],
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[2].silence",
        &["Did you mean `pause`? Write `pause: 300`"],
    );
    expect(
        &c,
        "unknown_field",
        "tracks[0].sequence[3].wait",
        &["Write `pause: \"1s\"`"],
    );
    // No contradictory "add the words to speak" for the same items.
    assert_eq!(count(&c, "missing_field"), 0, "{}", dump(&c));
}

// ---- values with one fix ----------------------------------------------------------------------------

#[test]
fn hints_convert_the_written_value() {
    let c = yaml(
        r#"version: "1.0"
output:
  path: out.mp3
  sample_rate: 44.1k
  loudness_preset: youtube
  target_lufs: 16
tracks:
  - id: bed
    type: music
    source: bed.wav
    volume: 50%
  - id: narration
    type: speech
    provider: mock
    sequence:
      - text: "Welcome to the show."
"#,
    );
    expect(
        &c,
        "invalid_type",
        "output.sample_rate",
        &["`sample_rate: 44100`"],
    );
    expect(
        &c,
        "invalid_type",
        "tracks[0].volume",
        &["`volume: 0.5` (50 % as a linear gain"],
    );
    expect(
        &c,
        "invalid_value",
        "output.target_lufs",
        &["Loudness values are negative: use `target_lufs: -16`"],
    );
    expect(
        &c,
        "invalid_value",
        "output.loudness_preset",
        &["`loudness_preset: streaming`"],
    );
    // Each mistake is reported once: no range error for 16, no unknown-preset error for youtube.
    assert_eq!(error_codes(&c).len(), 4, "{}", dump(&c));

    for (platform, preset) in [
        ("Spotify", "streaming"),
        ("TikTok", "streaming"),
        ("apple podcasts", "podcast"),
        ("TV", "broadcast"),
        ("EBU R128", "ebu_r128"),
    ] {
        let c = yaml(&BASE.replace(
            "tracks:",
            &format!("output: {{loudness_preset: \"{platform}\"}}\ntracks:"),
        ));
        expect(
            &c,
            "invalid_value",
            "output.loudness_preset",
            &[&format!("`loudness_preset: {preset}`")],
        );
    }
}

// ---- exact enum spellings ---------------------------------------------------------------------------

#[test]
fn case_and_dash_variants_of_enum_names_are_invalid() {
    let c = yaml(
        r#"version: "1.0"
output: {format: MP3, export_subtitles: VTT}
tracks:
  - id: v
    type: Speech
    provider: mock
    sequence:
      - text: hi
  - id: m
    type: music
    source: bed.wav
    fade_curve: equal-power
"#,
    );
    for (path, fixed) in [
        ("output.format", "Write `format: mp3`"),
        ("output.export_subtitles", "Write `export_subtitles: vtt`"),
        ("tracks[0].type", "Write `type: speech`"),
        ("tracks[1].fade_curve", "Write `fade_curve: equal_power`"),
    ] {
        expect(&c, "invalid_value", path, &["Did you mean", fixed]);
    }
    // One error per spelling; the speech track is still checked as a speech track.
    assert_eq!(error_codes(&c).len(), 4, "{}", dump(&c));
}

// ---- the time grammar and versions --------------------------------------------------------------------

#[test]
fn time_strings_follow_one_strict_grammar() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: v
    type: speech
    provider: mock
    sequence:
      - {text: a, start_at_ms: "1:1e2"}
      - {text: b, start_at_ms: "1:99.5"}
      - {text: c, start_at_ms: 350MS}
"#,
    );
    let d = find(&c, "invalid_value", "tracks[0].sequence[0].start_at_ms");
    assert!(d.message.contains("no exponents"), "{}", d.message);
    expect(
        &c,
        "invalid_value",
        "tracks[0].sequence[1].start_at_ms",
        &["Write `start_at_ms: \"159.5s\"`"],
    );
    let d = expect(
        &c,
        "invalid_value",
        "tracks[0].sequence[2].start_at_ms",
        &["Write `start_at_ms: \"350ms\"`"],
    );
    assert!(d.message.contains("units are lowercase"), "{}", d.message);
    assert_eq!(error_codes(&c).len(), 3, "{}", dump(&c));
}

#[test]
fn versions_with_leading_zeros_are_invalid() {
    let c = yaml(&BASE.replace("\"1.0\"", "\"01.0\""));
    expect(&c, "invalid_value", "version", &["version: \"1.0\""]);
}
