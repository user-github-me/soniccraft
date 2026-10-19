//! Time limits: every time value is finite and within ±24 h (`soniccraft_core::MAX_TIME_MS`), so validate,
//! plan and render never compute with absurd frame counts (a 1e12 ms start used to abort the process).

mod common;

use common::*;
use soniccraft_core::ir::{StartRule, checked_ms_to_frames};
use soniccraft_core::{MAX_TIME_MS, Severity};

#[test]
fn epoch_milliseconds_are_rejected_in_human_units() {
    let c = yaml(&BASE.replace(
        "provider: mock\n",
        "provider: mock\n        start_at_ms: 1700000000000\n",
    ));
    let d = expect(
        &c,
        "invalid_value",
        "tracks[0].sequence[0].start_at_ms",
        &[
            "epoch milliseconds",
            "seconds multiplied by 1000 twice",
            "`start_at_ms: \"1.5s\"`",
        ],
    );
    assert_eq!(
        d.message,
        "`start_at_ms` is 1700000000000 ms (53.9 years); the maximum is 24 h (86400000 ms)"
    );
    assert_eq!(d.severity, Severity::Error);
    assert!(c.program.is_none());
}

#[test]
fn huge_values_are_written_with_an_exponent() {
    let c = yaml(&BASE.replace(
        "provider: mock\n",
        "provider: mock\n        start_at_ms: 1e18\n",
    ));
    let d = find(&c, "invalid_value", "tracks[0].sequence[0].start_at_ms");
    assert_eq!(
        d.message,
        "`start_at_ms` is 1e18 ms (3.2e7 years); the maximum is 24 h (86400000 ms)"
    );
}

#[test]
fn every_time_field_is_limited() {
    let c = yaml(
        r#"version: "1.0"
output: {tail_ms: 1e12, duration_ms: "25h"}
subtitles: {max_duration_ms: 1e12, min_duration_ms: 1e12}
timing: {min_gap_ms: 1e12, overlap_tolerance_ms: 1e12}
tracks:
  - id: voice
    type: speech
    start_at_ms: 1e12
    trim_silence: {padding_ms: 1e12}
    sequence:
      - {id: a, text: "a", provider: mock, start_relative: {after_cue: b, offset_ms: -1e12}}
      - pause: 1e12
      - pause_duration: "1000h"
  - id: guest
    type: speech
    sequence:
      - {id: b, text: "b", provider: mock, start_at_ms: 1e12}
  - id: bed
    type: music
    clips:
      - {id: c1, source: a.wav, duration_ms: 1e12, fade_in_ms: 1e12, fade_out_ms: 1e12, trim_start_ms: 1e12, loop_crossfade_ms: 1e12}
      - {id: c2, source: b.wav, end_at_ms: 1e12, crossfade_ms: 1e12}
    ducking: {trigger_tracks: [voice], attack_ms: 1e12, release_ms: 1e12, hold_ms: 1e12, lookahead_ms: 1e12}
"#,
    );
    let paths = [
        "output.tail_ms",
        "output.duration_ms",
        "subtitles.max_duration_ms",
        "subtitles.min_duration_ms",
        "timing.min_gap_ms",
        "timing.overlap_tolerance_ms",
        "tracks[0].start_at_ms",
        "tracks[0].trim_silence.padding_ms",
        "tracks[0].sequence[1].pause",
        "tracks[0].sequence[2].pause_duration",
        "tracks[1].sequence[0].start_at_ms",
        "tracks[2].clips[0].duration_ms",
        "tracks[2].clips[0].fade_in_ms",
        "tracks[2].clips[0].fade_out_ms",
        "tracks[2].clips[0].trim_start_ms",
        "tracks[2].clips[0].loop_crossfade_ms",
        "tracks[2].clips[1].end_at_ms",
        "tracks[2].clips[1].crossfade_ms",
        "tracks[2].ducking.attack_ms",
        "tracks[2].ducking.release_ms",
        "tracks[2].ducking.hold_ms",
        "tracks[2].ducking.lookahead_ms",
    ];
    for path in paths {
        let d = find(&c, "invalid_value", path);
        assert!(
            d.message.contains("the maximum is 24 h (86400000 ms)"),
            "{path}: {}",
            d.message
        );
    }
    let offset = find(
        &c,
        "invalid_value",
        "tracks[0].sequence[0].start_relative.offset_ms",
    );
    assert_eq!(
        offset.message,
        "`offset_ms` is -1000000000000 ms (-31.7 years); offsets must be between -24 h and 24 h (±86400000 ms)"
    );
    assert!(
        find(&c, "invalid_value", "output.duration_ms")
            .message
            .contains("(25.0 h)")
    );
    assert!(
        find(&c, "invalid_value", "tracks[0].sequence[2].pause_duration")
            .message
            .contains("(41.7 days)")
    );
    assert_eq!(
        error_codes(&c),
        vec!["invalid_value"; paths.len() + 1],
        "{}",
        dump(&c)
    );
}

#[test]
fn exactly_24_hours_is_accepted() {
    let c = yaml(
        r#"version: "1.0"
output: {duration_ms: 86400000}
tracks:
  - id: voice
    type: speech
    sequence:
      - {id: a, text: "a", provider: mock, start_at_ms: "24h"}
      - {id: b, text: "b", provider: mock, start_relative: {after_cue: a, offset_ms: -86400000}}
"#,
    );
    assert_clean(&c);
    let p = c.program.expect("program");
    assert_eq!(p.output.duration_ms, Some(MAX_TIME_MS));
    assert_eq!(p.nodes[0].start, StartRule::Absolute { ms: MAX_TIME_MS });
    let StartRule::Relative(rule) = &p.nodes[1].start else {
        panic!("relative start")
    };
    assert_eq!(rule.offset_ms, -MAX_TIME_MS);
    // Every lowered time converts to frames without overflow, even at 192 kHz.
    assert_eq!(p.checked_frames(MAX_TIME_MS), Some(4_147_200_000));
    assert_eq!(
        checked_ms_to_frames(MAX_TIME_MS, 192_000),
        Some(16_588_800_000)
    );
    assert_eq!(checked_ms_to_frames(1e18, 48_000), None);
}

#[test]
fn just_over_24_hours_is_rejected() {
    let c = yaml(&BASE.replace("tracks:", "output: {duration_ms: 86400001}\ntracks:"));
    let d = find(&c, "invalid_value", "output.duration_ms");
    assert_eq!(
        d.message,
        "`duration_ms` is 86400001 ms (just over 24 h); the maximum is 24 h (86400000 ms)"
    );
}

#[test]
fn non_finite_times_are_rejected() {
    // `.inf`, `.nan` and numbers beyond f64 (`1e400`) never become a time.
    for value in [".inf", "-.inf", ".nan", "1e400"] {
        let c = yaml(&BASE.replace(
            "provider: mock\n",
            &format!("provider: mock\n        start_at_ms: {value}\n"),
        ));
        let d = find(&c, "invalid_value", "tracks[0].sequence[0].start_at_ms");
        assert_eq!(d.severity, Severity::Error, "{value}");
        assert_eq!(error_codes(&c), ["invalid_value"], "{value}:\n{}", dump(&c));
        assert!(c.program.is_none(), "{value}");
    }
}

#[test]
fn a_huge_start_with_ducking_is_a_diagnostic_not_a_crash() {
    // The reviewer's h2.yaml: validate used to allocate one byte per millisecond of timeline and abort.
    let c = yaml(
        r#"version: "1.0"
defaults: {provider: mock, voice_id: v}
tracks:
  - id: V
    type: speech
    sequence:
      - {id: v1, text: "Hello.", start_at_ms: 1e18}
  - id: bed
    type: music
    source: assets/audio/synthwave_loop.wav
    loop: true
    fade_out_ms: 1000
    ducking: {trigger_tracks: [V]}
"#,
    );
    assert_eq!(error_codes(&c), ["invalid_value"], "{}", dump(&c));
    assert!(c.program.is_none());
}
