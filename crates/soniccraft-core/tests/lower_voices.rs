//! Voices: speakers that share one voice, misspelled speakers, and voice-level keys written inside
//! `settings` (DESIGN.md §2.7).

mod common;

use common::*;
use soniccraft_core::Severity;

#[test]
fn speakers_that_share_the_default_mock_voice_are_warned() {
    // The agent-written dialogue: both speakers set only `provider: mock` and sound identical.
    let c = yaml(
        r#"version: "1.0"
speakers:
  alex: { provider: mock, label: Alex }
  sam: { provider: mock, label: Sam }
tracks:
  - id: talk
    type: speech
    sequence:
      - { id: a1, speaker: alex, text: "Cats sleep a lot." }
      - { id: s1, speaker: sam, text: "They really do." }
      - { id: a2, speaker: alex, text: "Indeed." }
"#,
    );
    assert_eq!(count(&c, "speakers_share_voice"), 1, "{}", dump(&c));
    let d = expect(
        &c,
        "speakers_share_voice",
        "tracks[0].sequence[1]",
        &[
            "`voice_id: alex` under `speakers.alex`",
            "`voice_id: sam` under `speakers.sam`",
            "any string works",
        ],
    );
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "speakers `alex` and `sam` both use the voice `mock` of provider `mock`, so they sound identical"
    );
    assert_eq!(d.related, ["a1", "s1"]);
    assert!(c.program.is_some(), "a warning does not block rendering");
}

#[test]
fn distinct_voice_ids_do_not_warn() {
    let c = yaml(
        r#"version: "1.0"
speakers:
  alex: { provider: mock, voice_id: alex }
  sam: { provider: mock, voice_id: sam }
tracks:
  - id: talk
    type: speech
    sequence:
      - { speaker: alex, text: "One." }
      - { speaker: sam, text: "Two." }
"#,
    );
    assert_eq!(count(&c, "speakers_share_voice"), 0, "{}", dump(&c));
    assert!(c.diagnostics.is_empty(), "{}", dump(&c));
}

#[test]
fn shared_voices_compare_provider_voice_model_and_settings() {
    let shared = |extra_sam: &str| {
        yaml(&format!(
            r#"version: "1.0"
defaults: {{provider: openai, voice_id: nova, settings: {{speed: 1.0, instructions: calm}}}}
speakers:
  host: {{}}
  guest: {{{extra_sam}}}
  third: {{settings: {{instructions: calm, speed: 1.0}}}}
tracks:
  - id: a
    type: speech
    speaker: host
    sequence: [{{text: "One."}}]
  - id: b
    type: speech
    sequence:
      - {{speaker: guest, text: "Two."}}
      - {{speaker: third, text: "Three."}}
"#
        ))
    };
    // Same voice for all three (settings key order does not matter): one warning listing them.
    let c = shared("");
    assert_eq!(count(&c, "speakers_share_voice"), 1, "{}", dump(&c));
    let d = expect(
        &c,
        "speakers_share_voice",
        "tracks[1].sequence[0]",
        &[
            "`voice_id: ...` under `speakers.guest`",
            "soniccraft voices openai",
        ],
    );
    assert!(
        d.message.starts_with(
            "speakers `host`, `guest` and `third` all use the voice `nova` of provider `openai`"
        ),
        "{}",
        d.message
    );
    assert!(
        d.message.contains("with the same settings"),
        "{}",
        d.message
    );
    // A different model, voice or setting is a different voice.
    for extra in [
        "model: tts-1-hd",
        "voice_id: echo",
        "settings: {speed: 1.2}",
    ] {
        let c = shared(extra);
        let d = find(&c, "speakers_share_voice", "tracks[1].sequence[1]");
        assert!(
            d.message
                .starts_with("speakers `host` and `third` both use"),
            "{extra}: {}",
            d.message
        );
    }
}

#[test]
fn shared_voice_hint_points_at_the_cues_when_they_set_the_voice() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: talk
    type: speech
    provider: mock
    voice_id: same
    sequence:
      - { speaker: alex, text: "One." }
      - { speaker: sam, text: "Two." }
"#,
    );
    expect(
        &c,
        "speakers_share_voice",
        "tracks[0].sequence[1]",
        &["`voice_id: sam` on the cues of `sam`"],
    );
}

#[test]
fn cues_without_speakers_do_not_count() {
    // Two narration tracks with one voice are not two speakers.
    let c = yaml(
        r#"version: "1.0"
defaults: {provider: mock}
tracks:
  - id: intro
    type: speech
    sequence: [{text: "One."}]
  - id: body
    type: speech
    sequence: [{text: "Two."}]
"#,
    );
    assert_eq!(count(&c, "speakers_share_voice"), 0, "{}", dump(&c));
}

#[test]
fn a_misspelled_speaker_is_an_error_without_cascading_voice_diagnostics() {
    // The reviewer's b08c: `narator` silently got the default voice and its own subtitle label.
    let c = yaml(
        r#"version: "1.0"
speakers:
  narrator: { provider: macos_say, voice_id: Daniel, label: Narrator }
  guest: { provider: macos_say, voice_id: Karen, label: Guest }
tracks:
  - id: dialog
    type: speech
    provider: macos_say
    sequence:
      - speaker: narator
        text: "Welcome to the show."
      - speaker: guest
        text: "Thanks for having me."
"#,
    );
    let d = expect(
        &c,
        "unknown_speaker",
        "tracks[0].sequence[0].speaker",
        &[
            "Did you mean `narrator`?",
            "`speaker: narrator`",
            "add `narator` under `speakers:`",
        ],
    );
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(error_codes(&c), ["unknown_speaker"], "{}", dump(&c));
    assert_eq!(count(&c, "default_voice"), 0, "{}", dump(&c));
    assert!(c.program.is_none());

    // On a track, every cue is affected. The voices are resolved as if the hint were applied, so the missing
    // provider (which `narrator` sets) is not a second error.
    let c = yaml(
        r#"version: "1.0"
speakers:
  narrator: {provider: mock}
tracks:
  - id: voice
    type: speech
    speaker: narator
    sequence: [{text: "a"}, {text: "b"}]
"#,
    );
    find(&c, "unknown_speaker", "tracks[0].speaker");
    assert_eq!(error_codes(&c), ["unknown_speaker"], "{}", dump(&c));

    // A voice problem that remains after applying the hint is reported in the same pass: `narrator` sets no
    // provider either.
    let c = yaml(
        r#"version: "1.0"
speakers:
  narrator: {voice_id: Daniel}
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: hi, text: "a", speaker: narator}
"#,
    );
    assert_eq!(
        error_codes(&c),
        ["missing_provider", "unknown_speaker"],
        "{}",
        dump(&c)
    );
    find(&c, "missing_provider", "tracks[0].sequence[0]");
}

#[test]
fn an_unregistered_speaker_without_a_close_match_is_a_label() {
    let c = yaml(
        r#"version: "1.0"
speakers:
  narrator: {provider: mock}
tracks:
  - id: voice
    type: speech
    provider: mock
    sequence:
      - {text: "a", speaker: narrator}
      - {text: "b", speaker: Zoe}
"#,
    );
    let d = expect(
        &c,
        "unknown_speaker",
        "tracks[0].sequence[1].speaker",
        &["Add `Zoe` under `speakers:`", "only a subtitle label"],
    );
    assert_eq!(d.severity, Severity::Warning);
    assert!(c.program.is_some());
}

#[test]
fn voice_keys_inside_settings_are_warned_with_a_move_hint() {
    // The reviewer's setvoice.yaml: the voice and model were silently ignored.
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: v
    type: speech
    sequence:
      - { id: a, text: "Hello from Karen.", provider: macos_say, settings: { voice: Karen } }
      - { id: b, text: "Hello from Nova.", provider: openai, settings: { voice: nova, model: tts-1-hd } }
"#,
    );
    let d = expect(
        &c,
        "field_not_applicable",
        "tracks[0].sequence[0].settings.voice",
        &["`voice_id: Karen`"],
    );
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "`settings.voice` is not a provider setting; the voice is chosen by `voice_id`"
    );
    expect(
        &c,
        "field_not_applicable",
        "tracks[0].sequence[1].settings.voice",
        &["Move it out of `settings`: `voice_id: nova`"],
    );
    let model = expect(
        &c,
        "field_not_applicable",
        "tracks[0].sequence[1].settings.model",
        &["Move it out of `settings`: `model: tts-1-hd`"],
    );
    assert_eq!(
        model.message,
        "`settings.model` is not a provider setting; the model is chosen by `model`"
    );
    assert_eq!(count(&c, "field_not_applicable"), 3);
    assert!(c.program.is_some(), "warnings only");
    // The warning is positioned at the key.
    assert_eq!(d.line, Some(6), "{d}");
}

#[test]
fn voice_keys_inside_settings_everywhere() {
    let c = yaml(
        r#"version: "1.0"
providers:
  studio: {type: openai, model: tts-1, settings: {voice: nova, model_id: tts-1-hd, language: en}}
  local: {type: piper, command: ["piper"], settings: {speaker: 3}}
defaults: {provider: studio, settings: {voice_name: "Karen (Enhanced)", speaker: narrator}}
speakers:
  host: {voice_id: onyx, settings: {voice: echo, speaker: host}}
tracks:
  - id: v
    type: speech
    speaker: host
    settings: {provider: mock, Language: de}
    sequence:
      - {text: "a", provider: local, voice_id: model.onnx, settings: {speaker: "2"}}
"#,
    );
    let hints = [
        (
            "providers.studio.settings.voice",
            "set `voice_id: nova` on the cues, tracks, speakers or `defaults` that use this provider",
        ),
        (
            "providers.studio.settings.model_id",
            "Remove `settings.model_id`; `model: tts-1` is already set next to it",
        ),
        (
            "providers.studio.settings.language",
            "set `language: en` on the cues, tracks, speakers or `defaults`",
        ),
        (
            "defaults.settings.voice_name",
            "Move it out of `settings`: `voice_id: \"Karen (Enhanced)\"`",
        ),
        (
            "defaults.settings.speaker",
            "set `speaker: narrator` on the cues or speech tracks",
        ),
        (
            "speakers.host.settings.voice",
            "Remove `settings.voice`; `voice_id: onyx` is already set next to it",
        ),
        (
            "speakers.host.settings.speaker",
            "this entry already is a speaker: cues and speech tracks use it with `speaker: host`",
        ),
        (
            "tracks[0].settings.provider",
            "Move it out of `settings`: `provider: mock`",
        ),
        (
            "tracks[0].settings.Language",
            "Move it out of `settings`: `language: de`",
        ),
    ];
    for (path, hint) in hints {
        expect(&c, "field_not_applicable", path, &[hint]);
    }
    // piper reads a numeric `settings.speaker`: never flagged.
    assert_eq!(
        count(&c, "field_not_applicable"),
        hints.len(),
        "{}",
        dump(&c)
    );
    let speaker = find(&c, "field_not_applicable", "defaults.settings.speaker");
    assert!(
        speaker
            .message
            .contains("piper reads only a numeric speaker id there"),
        "{}",
        speaker.message
    );
}

#[test]
fn provider_settings_are_not_flagged() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: v
    type: speech
    sequence:
      - {text: "a", provider: elevenlabs, voice_id: x, settings: {stability: 0.5, similarity_boost: 0.7, style: 0.1, speed: 1.0, language_code: en}}
      - {text: "b", provider: kokoro, settings: {speed: 1.1, lang_code: b}}
      - {text: "c", provider: cartesia, voice_id: y, settings: {locale: en-GB}}
      - {text: "d", provider: openai, settings: {instructions: "Speak calmly."}}
"#,
    );
    assert_eq!(count(&c, "field_not_applicable"), 0, "{}", dump(&c));
}
