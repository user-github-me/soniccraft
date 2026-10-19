//! The static dependency graph: track anchors in declaration order (DESIGN.md §2.13, the layout's model),
//! cycle messages through track anchors, and open-ended checks that ignore invalid lengths.

mod common;

use common::*;
use soniccraft_core::ir::{AnchorMode, AnchorTarget, StartRule};

#[test]
fn with_cue_on_a_track_reads_only_its_first_item() {
    // `whoosh` starts with the narration's first item; `line2` waits for `whoosh`. Not a cycle: `whoosh` does
    // not depend on `line2` (the layout places this as line1 [0, d1), whoosh [0, …), line2 after whoosh).
    let c = yaml(
        r#"version: "1.0"
defaults: {provider: mock, voice_id: v}
tracks:
  - id: narration
    type: speech
    sequence:
      - id: line1
        text: "Here is the first line."
      - id: line2
        start_relative: {after_cue: whoosh, offset_ms: 2000}
        text: "And this comes after the whoosh."
  - id: whoosh
    type: sfx
    source: assets/sfx/whoosh.wav
    start_relative: {with_cue: narration}
"#,
    );
    assert_eq!(count(&c, "dependency_cycle"), 0, "{}", dump(&c));
    assert_clean(&c);
    let p = c.program.expect("program");
    let StartRule::Relative(rule) = &p.tracks[1].start else {
        panic!("relative track start")
    };
    assert_eq!(
        (rule.target, rule.mode),
        (AnchorTarget::Track(0), AnchorMode::With)
    );
}

#[test]
fn track_anchored_with_another_track_whose_later_item_follows_it() {
    // The reviewer's track_anchor_cycle.yaml: intro starts with `voice` (= `hello`), `after_intro` follows
    // intro.
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    provider: mock
    sequence:
      - id: hello
        text: "Hello and welcome."
      - id: after_intro
        text: "Now the show begins."
        start_relative: {after_cue: intro}
  - id: intro
    type: music
    source: assets/sfx/chime.wav
    start_relative: {with_cue: voice}
"#,
    );
    assert_eq!(count(&c, "dependency_cycle"), 0, "{}", dump(&c));
    assert_clean(&c);
}

#[test]
fn after_cue_on_a_track_reads_only_its_last_item() {
    // `fx` starts after the end of `voice`'s last declared item `c` (absolute); `b` waits for `fx`. Edges to
    // every item of `voice` used to report `b → fx → b`.
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: a, text: "a", provider: mock}
      - {id: b, text: "b", provider: mock, start_relative: {after_cue: fx}}
      - {id: c, text: "c", provider: mock, start_at_ms: 5000}
  - id: fx
    type: sfx
    source: hit.wav
    start_relative: {after_cue: voice}
"#,
    );
    assert_eq!(count(&c, "dependency_cycle"), 0, "{}", dump(&c));
    assert_clean(&c);
}

#[test]
fn a_real_cycle_through_a_track_anchor_names_the_item_it_resolves_to() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: hello, text: "a", provider: mock}
      - {id: outro, text: "b", provider: mock, start_relative: {after_cue: sting}}
  - id: sting
    type: sfx
    source: hit.wav
    start_relative: {after_cue: voice}
"#,
    );
    assert_eq!(count(&c, "dependency_cycle"), 1, "{}", dump(&c));
    let d = expect(
        &c,
        "dependency_cycle",
        "tracks[0].sequence[1].start_relative.after_cue",
        &[
            "`after_cue: voice` follows the last declared item of track `voice` (`outro`)",
            "anchor to a specific cue",
        ],
    );
    assert_eq!(
        d.message,
        "dependency cycle: outro → sting → voice (track anchor: last item `outro`)"
    );
    assert_eq!(d.related, ["outro", "sting", "voice"]);
    assert!(c.program.is_none());
}

#[test]
fn a_cycle_through_a_with_cue_track_anchor_names_the_first_item() {
    let c = yaml(
        r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: hello, text: "a", provider: mock, start_relative: {after_cue: bed_end}}
  - id: bed
    type: music
    clips:
      - {id: bed_end, source: bed.wav, start_relative: {with_cue: voice}}
"#,
    );
    let d = find(
        &c,
        "dependency_cycle",
        "tracks[0].sequence[0].start_relative.after_cue",
    );
    assert!(
        d.message
            .contains("voice (track anchor: first item `hello`)"),
        "{}",
        d.message
    );
    assert!(
        d.message
            .starts_with("dependency cycle: hello → bed_end → "),
        "{}",
        d.message
    );
}

#[test]
fn node_cycles_keep_their_plain_message() {
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
    let d = find(
        &c,
        "dependency_cycle",
        "tracks[0].sequence[1].start_relative.after_cue",
    );
    assert_eq!(d.message, "dependency cycle: cue_02 → cue_03 → cue_02");
    assert!(!d.hint.as_deref().unwrap_or_default().contains("track"));
}

#[test]
fn an_invalid_loop_length_is_not_an_open_ended_loop() {
    // The reviewer's cascade.yaml: the bed's `duration_ms` is invalid; anchoring after the bed must not add
    // "`bed` is a looping clip without a length" (the user did give it a length).
    let c = yaml(
        r#"version: "1.0"
defaults: {provider: mock, voice_id: n}
tracks:
  - id: bed
    type: music
    source: assets/sfx/chime.wav
    loop: true
    duration_ms: "5 secs please"
  - id: v
    type: speech
    sequence:
      - id: c1
        text: "after the bed"
        start_relative: {after_cue: bed}
"#,
    );
    find(&c, "invalid_value", "tracks[0].duration_ms");
    assert_eq!(count(&c, "anchor_open_ended"), 0, "{}", dump(&c));
    assert_eq!(error_codes(&c), ["invalid_value"], "{}", dump(&c));
}

#[test]
fn a_dropped_loop_length_is_not_an_open_ended_loop() {
    for length in [
        "duration_ms: [5000]",
        "end_at_ms: {at: 5000}",
        "end_relative: {after_cue: nowhere}",
        "end_relative: {after_cue: [c1]}",
    ] {
        let c = yaml(&format!(
            r#"version: "1.0"
defaults: {{provider: mock, voice_id: n}}
tracks:
  - id: bed
    type: music
    source: bed.wav
    loop: true
    {length}
  - id: v
    type: speech
    sequence:
      - {{id: c1, text: "after the bed", start_relative: {{after_cue: bed}}}}
"#
        ));
        assert_eq!(count(&c, "anchor_open_ended"), 0, "{length}:\n{}", dump(&c));
        assert_eq!(error_codes(&c).len(), 1, "{length}:\n{}", dump(&c));
    }
}

#[test]
fn a_real_open_ended_anchor_lists_each_id_once() {
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
"#,
    );
    let d = find(
        &c,
        "anchor_open_ended",
        "tracks[1].sequence[0].start_relative.after_cue",
    );
    assert_eq!(d.related, ["bed"]);
}
