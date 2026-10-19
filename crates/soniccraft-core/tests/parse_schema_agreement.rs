//! Differential tests: the JSON Schema and the compiler agree on the structure of a timeline.
//!
//! Every document of the accepted corpus compiles without errors and validates against the schema. Every
//! document of the rejected corpus has one structural mistake: the compiler reports it (the expected code
//! at the expected path) and the schema rejects the document too. Nulls are included on purpose: the parser
//! reads `null` as an absent key, and the schema must do the same.

use serde_json::{Value, json};
use soniccraft_core::schema::timeline_schema;
use soniccraft_core::{Compiled, LowerOptions, SourceFormat, compile};

fn base() -> Value {
    json!({
        "version": "1.0",
        "tracks": [
            {"id": "v", "type": "speech", "provider": "mock", "voice_id": "alex", "sequence": [
                {"id": "a", "text": "Hello there."},
                {"pause": 300},
                {"id": "b", "text": "And welcome."}
            ]},
            {"id": "bed", "type": "music", "source": "bed.wav", "loop": true, "duration_ms": 20000,
             "fade_out_ms": 1000, "ducking": {"trigger_tracks": ["v"]}}
        ]
    })
}

/// `doc` with the value at the JSON pointer `pointer` set (the last segment may be a new key, or the
/// next index of a list).
fn set(mut doc: Value, pointer: &str, value: Value) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("pointer has a parent");
    match doc
        .pointer_mut(parent)
        .unwrap_or_else(|| panic!("no {parent}"))
    {
        Value::Object(map) => {
            map.insert(key.to_string(), value);
        }
        Value::Array(items) => {
            let i: usize = key.parse().expect("list index");
            if i == items.len() {
                items.push(value);
            } else {
                items[i] = value;
            }
        }
        _ => panic!("{parent} is not a mapping or list"),
    }
    doc
}

/// `doc` without the entry at `pointer`.
fn remove(mut doc: Value, pointer: &str) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("pointer has a parent");
    if let Some(Value::Object(map)) = doc.pointer_mut(parent) {
        map.shift_remove(key);
    }
    doc
}

/// Apply several `set`s.
fn with(doc: Value, changes: &[(&str, Value)]) -> Value {
    changes
        .iter()
        .fold(doc, |d, (pointer, value)| set(d, pointer, value.clone()))
}

fn compiled(doc: &Value) -> Compiled {
    let text = serde_json::to_string(doc).expect("serialize");
    compile(&text, SourceFormat::Json, &LowerOptions::default())
}

fn errors(c: &Compiled) -> Vec<String> {
    c.diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| format!("{d}"))
        .collect()
}

fn accepted() -> Vec<(&'static str, Value)> {
    let speech_cue = |extra: Value| {
        let mut cue = json!({"text": "Extra cue."});
        if let (Value::Object(cue), Value::Object(extra)) = (&mut cue, extra) {
            cue.extend(extra);
        }
        cue
    };
    vec![
        ("base", base()),
        (
            "nulls mean absent",
            with(
                base(),
                &[
                    ("/output", Value::Null),
                    ("/speakers", Value::Null),
                    ("/providers", Value::Null),
                    ("/tracks/1/volume", Value::Null),
                    ("/tracks/1/fade_in_ms", Value::Null),
                    ("/tracks/1/sequence", Value::Null),
                    ("/tracks/1/clips", Value::Null),
                    ("/tracks/0/source", Value::Null),
                    ("/tracks/0/sequence/0/voice_id", Value::Null),
                    ("/tracks/0/sequence/0/start_at_ms", json!(100)),
                    ("/tracks/0/sequence/0/start_relative", Value::Null),
                    (
                        "/tracks/0/sequence/1",
                        json!({"pause": 300, "pause_duration": null, "text": null, "speaker": null, "x-why": "breath"}),
                    ),
                    (
                        "/tracks/0/sequence/2/start_relative",
                        json!({"after_cue": "a", "with_cue": null, "before_cue": null, "offset_ms": 150}),
                    ),
                ],
            ),
        ),
        (
            "end_relative with a null before_cue",
            with(
                remove(base(), "/tracks/1/duration_ms"),
                &[(
                    "/tracks/1/end_relative",
                    json!({"after_cue": "b", "before_cue": null, "offset_ms": "2s"}),
                )],
            ),
        ),
        (
            "signed offsets, zero and the 24 h bound",
            with(
                base(),
                &[
                    (
                        "/tracks/0/sequence/2/start_relative",
                        json!({"with_cue": "a", "offset_ms": -86400000}),
                    ),
                    ("/tracks/1/fade_in_ms", json!("-0ms")),
                    ("/tracks/1/trim_start_ms", json!(86400000)),
                    ("/tracks/1/loop_crossfade_ms", json!("0:00.25")),
                    ("/tracks/0/sequence/1/pause", json!("00:00:01.500")),
                ],
            ),
        ),
        (
            "clips list with aliases",
            set(
                base(),
                "/tracks/1",
                json!({"id": "bed", "type": "music", "clips": [
                    {"id": "intro", "source": "a.wav", "duration_ms": "5s", "fade_curve": "sine"},
                    {"source": "b.wav", "crossfade_ms": 500, "end_relative": {"with_cue": "b"}}
                ], "ducking": {"trigger_tracks": ["v"], "mode": "timeline"}}),
            ),
        ),
        (
            "before_cue starts with a length",
            with(
                base(),
                &[
                    (
                        "/tracks/2",
                        json!({"id": "whoosh", "type": "sfx", "source": "w.wav",
                               "start_relative": {"before_cue": "b", "offset_ms": -100}, "duration_ms": 400}),
                    ),
                    (
                        "/tracks/3",
                        json!({"id": "loops", "type": "sfx", "clips": [
                            {"source": "x.wav", "loop": true, "duration_ms": 1000, "start_relative": {"before_cue": "b"}}
                        ]}),
                    ),
                ],
            ),
        ),
        (
            "output aliases and enums",
            with(
                base(),
                &[
                    (
                        "/output",
                        json!({"format": "aac", "loudness_preset": "ebu_r128", "target_lufs": -60,
                               "true_peak_db": -9, "export_subtitles": ["webvtt", "words", "hyperframes"],
                               "tail_ms": 0, "duration_ms": 1, "normalize": false, "dither": true}),
                    ),
                    (
                        "/timing",
                        json!({"speech_overlap": "allow", "min_gap_ms": 0, "overlap_tolerance_ms": "20ms"}),
                    ),
                    ("/mix", json!({"reference_lufs": -70})),
                    (
                        "/subtitles",
                        json!({"max_duration_ms": "6s", "min_duration_ms": 0, "max_chars_per_line": 1,
                               "max_lines": 1, "speaker_labels": true}),
                    ),
                ],
            ),
        ),
        (
            "providers by name and by type",
            with(
                base(),
                &[
                    (
                        "/providers",
                        json!({
                            "openai": {"base_url": "http://localhost:8880/v1", "api_key_env": "OPENAI_API_KEY"},
                            "eleven": {"type": "eleven_labs", "api_key_env": " ELEVEN_KEY "},
                            "studio": {"type": "command", "command": ["my-tts", "{output}"], "stdin": "{text}"},
                            "piper": {"command": ["piper", "-m", "{voice_id}", "-f", "{output}"], "voices_dir": "v"},
                            "mock": {"api_key_env": "not a variable", "command": []},
                            "say": {"command": []},
                            "11labs": null,
                            "x-note": "annotations are not providers"
                        }),
                    ),
                    ("/tracks/0/sequence/2/provider", json!("studio")),
                    ("/tracks/0/sequence/2/voice_id", json!("x")),
                ],
            ),
        ),
        (
            "bitrates of lossless outputs are ignored",
            set(
                base(),
                "/output",
                json!({"format": "wav", "bitrate": "loud"}),
            ),
        ),
        (
            "bitrates of lossless outputs by extension are ignored",
            set(
                base(),
                "/output",
                json!({"path": "out.flac", "bitrate": "99999k"}),
            ),
        ),
        (
            "lossy bitrates",
            set(
                base(),
                "/output",
                json!({"format": "mp3", "bitrate": "320k"}),
            ),
        ),
        (
            "lossy bitrates by extension",
            set(
                base(),
                "/output",
                json!({"path": "out/ep.MP3", "bitrate": "192 kbps"}),
            ),
        ),
        (
            "kbps numbers",
            set(base(), "/output", json!({"format": "opus", "bitrate": 96})),
        ),
        (
            "speakers and voices",
            with(
                base(),
                &[
                    (
                        "/speakers",
                        json!({"narrator": null, "guest": {"provider": "mock", "voice_id": "sam", "label": "Guest",
                               "gain_db": -3, "settings": {"x-anything": 1}}, "x-cast": "notes"}),
                    ),
                    ("/defaults", json!({"provider": "mock", "language": "en"})),
                    (
                        "/tracks/0/sequence/3",
                        speech_cue(
                            json!({"speaker": "guest", "caption": "Extra", "subtitles": false,
                                          "allow_overlap": true, "settings": {"speed": 1.1}, "model": "m",
                                          "start_at_ms": "1m 30.5s", "volume": 4, "gain_db": 24}),
                        ),
                    ),
                ],
            ),
        ),
        ("version \"1\"", set(base(), "/version", json!("1"))),
        ("version 1", set(base(), "/version", json!(1))),
        ("version 1.0", set(base(), "/version", json!(1.0))),
        ("version \"v1.0\"", set(base(), "/version", json!("v1.0"))),
        (
            "version 1.2 (warning)",
            set(base(), "/version", json!("1.2")),
        ),
        (
            "subtitle switches",
            set(base(), "/output", json!({"export_subtitles": true})),
        ),
        (
            "one subtitle format",
            set(base(), "/output", json!({"export_subtitles": "srt"})),
        ),
        (
            "no subtitle formats",
            set(base(), "/output", json!({"export_subtitles": []})),
        ),
        (
            "flag-or-object values",
            with(
                base(),
                &[
                    ("/tracks/1/normalize", json!({})),
                    (
                        "/tracks/0/normalize",
                        json!({"target_lufs": -70, "x-why": "quiet"}),
                    ),
                    (
                        "/tracks/0/trim_silence",
                        json!({"padding_ms": "30ms", "threshold_db": -100}),
                    ),
                ],
            ),
        ),
        (
            "a track of pauses only (warning)",
            set(
                base(),
                "/tracks/2",
                json!({"id": "v2", "type": "speech", "provider": "mock", "voice_id": "x", "sequence": [{"pause": "1s"}]}),
            ),
        ),
        (
            "ducking with every field",
            set(
                base(),
                "/tracks/1/ducking",
                json!({"trigger_tracks": ["v"], "reduction_db": 60, "attack_ms": 0, "release_ms": "1s",
                       "hold_ms": 250, "lookahead_ms": 0, "threshold_db": -100, "mode": "cue"}),
            ),
        ),
        (
            "annotations everywhere",
            with(
                base(),
                &[
                    ("/x-generator", json!("agent")),
                    ("/tracks/0/x_color", json!([1, 2])),
                    ("/tracks/0/sequence/0/x-take", json!(2)),
                    ("/tracks/1/ducking/x-note", json!({"a": 1})),
                ],
            ),
        ),
    ]
}

/// (name, document, expected error code, expected error path).
fn rejected() -> Vec<(&'static str, Value, &'static str, &'static str)> {
    let start_conflict = json!({"start_at_ms": 0, "start_relative": {"after_cue": "a"}});
    let merge = |doc: Value, pointer: &str, extra: &Value| {
        let mut doc = doc;
        if let (Some(Value::Object(target)), Value::Object(extra)) =
            (doc.pointer_mut(pointer), extra)
        {
            target.extend(extra.clone());
        }
        doc
    };
    let sfx = |fields: Value| {
        let mut track = json!({"id": "fx", "type": "sfx", "source": "fx.wav"});
        if let (Value::Object(t), Value::Object(f)) = (&mut track, fields) {
            t.extend(f);
        }
        set(base(), "/tracks/2", track)
    };
    let clip = |fields: Value| {
        let mut c = json!({"source": "a.wav"});
        if let (Value::Object(c), Value::Object(f)) = (&mut c, fields) {
            c.extend(f);
        }
        set(
            base(),
            "/tracks/1",
            json!({"id": "bed", "type": "music", "clips": [c]}),
        )
    };
    let provider = |name: &str, spec: Value| set(base(), "/providers", json!({ name: spec }));
    let output = |spec: Value| set(base(), "/output", spec);
    vec![
        // ---- conflicts ----
        (
            "track start_at_ms + start_relative",
            merge(base(), "/tracks/1", &start_conflict),
            "conflicting_fields",
            "tracks[1].start_relative",
        ),
        (
            "cue start_at_ms + start_relative",
            merge(base(), "/tracks/0/sequence/2", &start_conflict),
            "conflicting_fields",
            "tracks[0].sequence[2].start_relative",
        ),
        (
            "clip start_at_ms + start_relative",
            clip(start_conflict.clone()),
            "conflicting_fields",
            "tracks[1].clips[0].start_relative",
        ),
        (
            "duration_ms + end_at_ms",
            set(base(), "/tracks/1/end_at_ms", json!(30000)),
            "conflicting_fields",
            "tracks[1].end_at_ms",
        ),
        (
            "clip duration_ms + end_relative",
            clip(json!({"duration_ms": 1000, "end_relative": {"after_cue": "b"}})),
            "conflicting_fields",
            "tracks[1].clips[0].end_relative",
        ),
        (
            "end_at_ms + end_relative",
            with(
                remove(base(), "/tracks/1/duration_ms"),
                &[
                    ("/tracks/1/end_at_ms", json!(9000)),
                    ("/tracks/1/end_relative", json!({"after_cue": "b"})),
                ],
            ),
            "conflicting_fields",
            "tracks[1].end_relative",
        ),
        (
            "clips + single-clip fields",
            set(base(), "/tracks/1/clips", json!([{"source": "b.wav"}])),
            "conflicting_fields",
            "tracks[1].source",
        ),
        (
            "clips + loop",
            set(
                base(),
                "/tracks/1",
                json!({"id": "bed", "type": "music", "loop": true, "clips": [{"source": "b.wav"}]}),
            ),
            "conflicting_fields",
            "tracks[1].loop",
        ),
        (
            "before_cue start + end_at_ms",
            sfx(json!({"start_relative": {"before_cue": "b"}, "end_at_ms": 5000})),
            "conflicting_fields",
            "tracks[2].end_at_ms",
        ),
        (
            "before_cue start + open-ended loop",
            sfx(json!({"start_relative": {"before_cue": "b"}, "loop": true})),
            "conflicting_fields",
            "tracks[2].start_relative.before_cue",
        ),
        (
            "pause + text",
            set(base(), "/tracks/0/sequence/0/pause", json!(100)),
            "conflicting_fields",
            "tracks[0].sequence[0].pause",
        ),
        (
            "pause + speaker",
            set(base(), "/tracks/0/sequence/1/speaker", json!("n")),
            "conflicting_fields",
            "tracks[0].sequence[1].speaker",
        ),
        (
            "pause + pause_duration",
            set(base(), "/tracks/0/sequence/1/pause_duration", json!(100)),
            "conflicting_fields",
            "tracks[0].sequence[1].pause",
        ),
        (
            "pause with an id",
            set(base(), "/tracks/0/sequence/1/id", json!("p")),
            "conflicting_fields",
            "tracks[0].sequence[1].id",
        ),
        (
            "anchor with two targets",
            set(
                base(),
                "/tracks/0/sequence/2/start_relative",
                json!({"after_cue": "a", "with_cue": "a"}),
            ),
            "conflicting_fields",
            "tracks[0].sequence[2].start_relative.with_cue",
        ),
        (
            "anchor without a target",
            set(
                base(),
                "/tracks/0/sequence/2/start_relative",
                json!({"offset_ms": 5}),
            ),
            "conflicting_fields",
            "tracks[0].sequence[2].start_relative",
        ),
        (
            "before_cue in end_relative",
            with(
                remove(base(), "/tracks/1/duration_ms"),
                &[("/tracks/1/end_relative", json!({"before_cue": "b"}))],
            ),
            "conflicting_fields",
            "tracks[1].end_relative.before_cue",
        ),
        // ---- field applicability ----
        (
            "speech track with source",
            set(base(), "/tracks/0/source", json!("x.wav")),
            "field_not_applicable",
            "tracks[0].source",
        ),
        (
            "speech track with clips",
            set(base(), "/tracks/0/clips", json!([{"source": "x.wav"}])),
            "field_not_applicable",
            "tracks[0].clips",
        ),
        (
            "speech track with a fade",
            set(base(), "/tracks/0/fade_in_ms", json!(100)),
            "field_not_applicable",
            "tracks[0].fade_in_ms",
        ),
        (
            "speech track with end_at_ms",
            set(base(), "/tracks/0/end_at_ms", json!(1000)),
            "field_not_applicable",
            "tracks[0].end_at_ms",
        ),
        (
            "speech track with loop",
            set(base(), "/tracks/0/loop", json!(true)),
            "field_not_applicable",
            "tracks[0].loop",
        ),
        (
            "music track with a sequence",
            set(base(), "/tracks/1/sequence", json!([{"text": "x"}])),
            "field_not_applicable",
            "tracks[1].sequence",
        ),
        (
            "music track with voice_id",
            set(base(), "/tracks/1/voice_id", json!("x")),
            "field_not_applicable",
            "tracks[1].voice_id",
        ),
        (
            "music track with trim_silence",
            set(base(), "/tracks/1/trim_silence", json!(false)),
            "field_not_applicable",
            "tracks[1].trim_silence",
        ),
        (
            "sfx track with a speaker",
            sfx(json!({"speaker": "n"})),
            "field_not_applicable",
            "tracks[2].speaker",
        ),
        // ---- time values ----
        (
            "negative pause",
            set(base(), "/tracks/0/sequence/1/pause", json!(-100)),
            "invalid_value",
            "tracks[0].sequence[1].pause",
        ),
        (
            "negative start",
            set(base(), "/tracks/0/sequence/0/start_at_ms", json!(-5)),
            "invalid_value",
            "tracks[0].sequence[0].start_at_ms",
        ),
        (
            "negative start string",
            set(base(), "/tracks/0/sequence/0/start_at_ms", json!("-5ms")),
            "invalid_value",
            "tracks[0].sequence[0].start_at_ms",
        ),
        (
            "negative fade",
            set(base(), "/tracks/1/fade_in_ms", json!(-1)),
            "invalid_value",
            "tracks[1].fade_in_ms",
        ),
        (
            "zero clip length",
            set(base(), "/tracks/1/duration_ms", json!(0)),
            "invalid_value",
            "tracks[1].duration_ms",
        ),
        (
            "zero clip length string",
            set(base(), "/tracks/1/duration_ms", json!("0s")),
            "invalid_value",
            "tracks[1].duration_ms",
        ),
        (
            "zero output length",
            output(json!({"duration_ms": 0})),
            "invalid_value",
            "output.duration_ms",
        ),
        (
            "zero caption length",
            set(base(), "/subtitles", json!({"max_duration_ms": 0})),
            "invalid_value",
            "subtitles.max_duration_ms",
        ),
        (
            "negative gap",
            set(base(), "/timing", json!({"min_gap_ms": -1})),
            "invalid_value",
            "timing.min_gap_ms",
        ),
        (
            "negative ducking attack",
            set(base(), "/tracks/1/ducking/attack_ms", json!("-10ms")),
            "invalid_value",
            "tracks[1].ducking.attack_ms",
        ),
        (
            "clock time with an exponent",
            set(base(), "/tracks/0/sequence/0/start_at_ms", json!("1:1e2")),
            "invalid_value",
            "tracks[0].sequence[0].start_at_ms",
        ),
        (
            "clock seconds of 60 or more",
            set(base(), "/tracks/1/fade_in_ms", json!("1:99.5")),
            "invalid_value",
            "tracks[1].fade_in_ms",
        ),
        (
            "clock minutes of 60 or more",
            set(base(), "/tracks/1/fade_in_ms", json!("1:75:00")),
            "invalid_value",
            "tracks[1].fade_in_ms",
        ),
        (
            "uppercase unit",
            set(base(), "/tracks/1/fade_in_ms", json!("350MS")),
            "invalid_value",
            "tracks[1].fade_in_ms",
        ),
        (
            "unparsable duration",
            set(base(), "/tracks/1/fade_in_ms", json!("5 parsecs")),
            "invalid_value",
            "tracks[1].fade_in_ms",
        ),
        // ---- required keys and nulls ----
        (
            "null version",
            set(base(), "/version", Value::Null),
            "missing_field",
            "version",
        ),
        (
            "missing version",
            remove(base(), "/version"),
            "missing_field",
            "version",
        ),
        (
            "null track id",
            set(base(), "/tracks/0/id", Value::Null),
            "missing_field",
            "tracks[0].id",
        ),
        (
            "null track type",
            set(base(), "/tracks/0/type", Value::Null),
            "missing_field",
            "tracks[0].type",
        ),
        (
            "null cue text",
            set(base(), "/tracks/0/sequence/0/text", Value::Null),
            "missing_field",
            "tracks[0].sequence[0].text",
        ),
        (
            "empty sequence item",
            set(base(), "/tracks/0/sequence/3", json!({})),
            "missing_field",
            "tracks[0].sequence[3].text",
        ),
        (
            "null sequence",
            set(base(), "/tracks/0/sequence", Value::Null),
            "missing_field",
            "tracks[0].sequence",
        ),
        (
            "null trigger_tracks",
            set(base(), "/tracks/1/ducking/trigger_tracks", Value::Null),
            "missing_field",
            "tracks[1].ducking.trigger_tracks",
        ),
        (
            "null track source without clips",
            set(base(), "/tracks/1/source", Value::Null),
            "missing_field",
            "tracks[1].source",
        ),
        (
            "null clip source",
            clip(json!({"source": null})),
            "missing_field",
            "tracks[1].clips[0].source",
        ),
        (
            "track without type",
            remove(base(), "/tracks/0/type"),
            "missing_field",
            "tracks[0].type",
        ),
        // ---- empty strings ----
        (
            "blank track source",
            set(base(), "/tracks/1/source", json!(" ")),
            "invalid_value",
            "tracks[1].source",
        ),
        (
            "empty caption",
            set(base(), "/tracks/0/sequence/0/caption", json!("")),
            "empty_text",
            "tracks[0].sequence[0].caption",
        ),
        (
            "blank text",
            set(base(), "/tracks/0/sequence/0/text", json!("   ")),
            "empty_text",
            "tracks[0].sequence[0].text",
        ),
        (
            "empty speaker",
            set(base(), "/tracks/0/sequence/0/speaker", json!("")),
            "invalid_value",
            "tracks[0].sequence[0].speaker",
        ),
        (
            "empty cue voice_id",
            set(base(), "/tracks/0/sequence/0/voice_id", json!("")),
            "invalid_value",
            "tracks[0].sequence[0].voice_id",
        ),
        (
            "blank track provider",
            set(base(), "/tracks/0/provider", json!("  ")),
            "invalid_value",
            "tracks[0].provider",
        ),
        (
            "empty defaults voice_id",
            set(base(), "/defaults", json!({"voice_id": ""})),
            "invalid_value",
            "defaults.voice_id",
        ),
        (
            "empty anchor target",
            set(
                base(),
                "/tracks/0/sequence/2/start_relative",
                json!({"after_cue": ""}),
            ),
            "unknown_reference",
            "tracks[0].sequence[2].start_relative.after_cue",
        ),
        (
            "empty trigger track",
            set(base(), "/tracks/1/ducking/trigger_tracks", json!([""])),
            "unknown_reference",
            "tracks[1].ducking.trigger_tracks[0]",
        ),
        (
            "empty output path",
            output(json!({"path": ""})),
            "invalid_value",
            "output.path",
        ),
        // ---- exact enum spellings ----
        (
            "capitalized track type",
            set(base(), "/tracks/0/type", json!("Speech")),
            "invalid_value",
            "tracks[0].type",
        ),
        (
            "uppercase output format",
            output(json!({"format": "MP3"})),
            "invalid_value",
            "output.format",
        ),
        (
            "dash in a fade curve",
            set(base(), "/tracks/1/fade_curve", json!("equal-power")),
            "invalid_value",
            "tracks[1].fade_curve",
        ),
        (
            "uppercase subtitle format",
            output(json!({"export_subtitles": "VTT"})),
            "invalid_value",
            "output.export_subtitles",
        ),
        (
            "capitalized loudness preset",
            output(json!({"loudness_preset": "Podcast"})),
            "invalid_value",
            "output.loudness_preset",
        ),
        (
            "platform as loudness preset",
            output(json!({"loudness_preset": "youtube"})),
            "invalid_value",
            "output.loudness_preset",
        ),
        (
            "uppercase overlap policy",
            set(base(), "/timing", json!({"speech_overlap": "PUSH"})),
            "invalid_value",
            "timing.speech_overlap",
        ),
        (
            "capitalized ducking mode",
            set(base(), "/tracks/1/ducking/mode", json!("Cue")),
            "invalid_value",
            "tracks[1].ducking.mode",
        ),
        (
            "camel-case provider type",
            provider("x", json!({"type": "OpenAI"})),
            "invalid_value",
            "providers.x.type",
        ),
        (
            "padded subtitle format",
            output(json!({"export_subtitles": [" srt"]})),
            "invalid_value",
            "output.export_subtitles[0]",
        ),
        // ---- versions ----
        (
            "version with a leading zero",
            set(base(), "/version", json!("01.0")),
            "invalid_value",
            "version",
        ),
        (
            "version with three parts",
            set(base(), "/version", json!("1.0.0")),
            "invalid_value",
            "version",
        ),
        (
            "version 2",
            set(base(), "/version", json!("2.0")),
            "unsupported_version",
            "version",
        ),
        // ---- providers ----
        (
            "custom provider name without type",
            provider("myvoice", json!({"base_url": "http://x"})),
            "missing_field",
            "providers.myvoice.type",
        ),
        (
            "command provider without command",
            provider("c", json!({"type": "command"})),
            "missing_field",
            "providers.c.command",
        ),
        (
            "null command provider",
            provider("command", Value::Null),
            "missing_field",
            "providers.command.command",
        ),
        (
            "empty command",
            provider("c", json!({"type": "command", "command": []})),
            "invalid_value",
            "providers.c.command",
        ),
        (
            "blank program",
            provider("piper", json!({"command": [" ", "x"]})),
            "invalid_value",
            "providers.piper.command",
        ),
        (
            "api_key_env that is not a variable name",
            provider("openai", json!({"api_key_env": "my key"})),
            "invalid_value",
            "providers.openai.api_key_env",
        ),
        (
            "base_url without scheme",
            provider("k", json!({"type": "kokoro", "base_url": "localhost:8880"})),
            "invalid_value",
            "providers.k.base_url",
        ),
        (
            "base_url without host",
            provider("openai", json!({"base_url": "http:///v1"})),
            "invalid_value",
            "providers.openai.base_url",
        ),
        (
            "zero concurrency",
            provider("mock", json!({"max_concurrency": 0})),
            "invalid_value",
            "providers.mock.max_concurrency",
        ),
        (
            "zero timeout",
            provider("mock", json!({"timeout_ms": 0})),
            "invalid_value",
            "providers.mock.timeout_ms",
        ),
        (
            "zero speaking rate",
            provider("mock", json!({"chars_per_second": 0})),
            "invalid_value",
            "providers.mock.chars_per_second",
        ),
        // ---- output ----
        (
            "huge mp3 bitrate",
            output(json!({"format": "mp3", "bitrate": "99999k"})),
            "invalid_value",
            "output.bitrate",
        ),
        (
            "huge bitrate by extension",
            output(json!({"path": "x.mp3", "bitrate": "99999k"})),
            "invalid_value",
            "output.bitrate",
        ),
        (
            "bitrate word",
            output(json!({"format": "ogg", "bitrate": "loud"})),
            "invalid_value",
            "output.bitrate",
        ),
        (
            "zero bitrate",
            output(json!({"format": "mp3", "bitrate": 0})),
            "invalid_value",
            "output.bitrate",
        ),
        (
            "bitrate with an exponent",
            output(json!({"format": "m4a", "bitrate": "1e3"})),
            "invalid_value",
            "output.bitrate",
        ),
        (
            "sample rate out of range",
            output(json!({"sample_rate": 1000})),
            "unsupported_sample_rate",
            "output.sample_rate",
        ),
        (
            "three channels",
            output(json!({"channels": 3})),
            "invalid_value",
            "output.channels",
        ),
        (
            "bit depth 20",
            output(json!({"bit_depth": 20})),
            "unsupported_bit_depth",
            "output.bit_depth",
        ),
        (
            "loudness target above -5",
            output(json!({"target_lufs": -4})),
            "invalid_value",
            "output.target_lufs",
        ),
        (
            "positive loudness target",
            output(json!({"target_lufs": 16})),
            "invalid_value",
            "output.target_lufs",
        ),
        (
            "positive true peak",
            output(json!({"true_peak_db": 1})),
            "invalid_value",
            "output.true_peak_db",
        ),
        // ---- ranges and lists ----
        (
            "volume above 4",
            set(base(), "/tracks/1/volume", json!(5)),
            "invalid_value",
            "tracks[1].volume",
        ),
        (
            "pan beyond 1",
            set(base(), "/tracks/1/pan", json!(2)),
            "invalid_value",
            "tracks[1].pan",
        ),
        (
            "cue gain above 24 dB",
            set(base(), "/tracks/0/sequence/0/gain_db", json!(30)),
            "invalid_value",
            "tracks[0].sequence[0].gain_db",
        ),
        (
            "normalize target out of range",
            set(base(), "/tracks/0/normalize", json!({"target_lufs": -100})),
            "invalid_value",
            "tracks[0].normalize.target_lufs",
        ),
        (
            "ducking deeper than 60 dB",
            set(base(), "/tracks/1/ducking/reduction_db", json!(61)),
            "invalid_value",
            "tracks[1].ducking.reduction_db",
        ),
        (
            "positive ducking threshold",
            set(base(), "/tracks/1/ducking/threshold_db", json!(10)),
            "invalid_value",
            "tracks[1].ducking.threshold_db",
        ),
        (
            "zero subtitle lines",
            set(base(), "/subtitles", json!({"max_lines": 0})),
            "invalid_value",
            "subtitles.max_lines",
        ),
        (
            "no trigger tracks",
            set(base(), "/tracks/1/ducking/trigger_tracks", json!([])),
            "invalid_value",
            "tracks[1].ducking.trigger_tracks",
        ),
        (
            "no tracks",
            set(base(), "/tracks", json!([])),
            "empty_tracks",
            "tracks",
        ),
        (
            "empty sequence",
            set(base(), "/tracks/0/sequence", json!([])),
            "empty_sequence",
            "tracks[0].sequence",
        ),
        (
            "empty clips list",
            set(
                base(),
                "/tracks/1",
                json!({"id": "bed", "type": "music", "clips": []}),
            ),
            "empty_sequence",
            "tracks[1].clips",
        ),
        // ---- keys, ids and types ----
        (
            "unknown key",
            set(base(), "/tracks/1/fade_in", json!(100)),
            "unknown_field",
            "tracks[1].fade_in",
        ),
        (
            "id with a space",
            set(base(), "/tracks/1/id", json!("my bed")),
            "invalid_id",
            "tracks[1].id",
        ),
        (
            "cue id starting with a dash",
            set(base(), "/tracks/0/sequence/0/id", json!("-a")),
            "invalid_id",
            "tracks[0].sequence[0].id",
        ),
        (
            "id with surrounding spaces",
            set(base(), "/tracks/1/id", json!(" bed ")),
            "invalid_id",
            "tracks[1].id",
        ),
        // ---- the 24 h bound of time values ----
        (
            "start after 24 h",
            set(base(), "/tracks/1/start_at_ms", json!(86_400_001)),
            "invalid_value",
            "tracks[1].start_at_ms",
        ),
        (
            "epoch milliseconds",
            set(base(), "/tracks/0/sequence/0/start_at_ms", json!(1.7e12)),
            "invalid_value",
            "tracks[0].sequence[0].start_at_ms",
        ),
        (
            "offset before -24 h",
            set(
                base(),
                "/tracks/0/sequence/2/start_relative",
                json!({"after_cue": "a", "offset_ms": -86_400_001}),
            ),
            "invalid_value",
            "tracks[0].sequence[2].start_relative.offset_ms",
        ),
        (
            "speaker that is not a mapping",
            set(base(), "/speakers", json!({"n": "Narrator"})),
            "invalid_type",
            "speakers.n",
        ),
        (
            "settings that are not a mapping",
            set(base(), "/tracks/0/settings", json!(5)),
            "invalid_type",
            "tracks[0].settings",
        ),
    ]
}

#[test]
fn accepted_documents_compile_and_validate() {
    let validator = jsonschema::draft202012::new(&timeline_schema()).expect("schema compiles");
    let mut failures = Vec::new();
    for (name, doc) in accepted() {
        let c = compiled(&doc);
        let errs = errors(&c);
        if !errs.is_empty() {
            failures.push(format!("{name}: compile reports errors: {errs:#?}"));
        }
        let schema_errors: Vec<String> = validator
            .iter_errors(&doc)
            .map(|e| format!("{e} at `{}`", e.instance_path()))
            .collect();
        if !schema_errors.is_empty() {
            failures.push(format!("{name}: the schema rejects it: {schema_errors:#?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn structural_errors_are_rejected_by_both() {
    let validator = jsonschema::draft202012::new(&timeline_schema()).expect("schema compiles");
    let mut failures = Vec::new();
    for (name, doc, code, path) in rejected() {
        let c = compiled(&doc);
        let reported = c
            .diagnostics
            .iter()
            .any(|d| d.is_error() && d.code == code && d.path.as_deref() == Some(path));
        if !reported {
            failures.push(format!(
                "{name}: compile does not report `{code}` at `{path}`: {:#?}",
                errors(&c)
            ));
        }
        if validator.is_valid(&doc) {
            failures.push(format!("{name}: the schema accepts it"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// What only the compiler can check (references between ids, duration strings beyond 24 hours, the output
/// extension, format-specific sample rates): the schema accepts these documents, the compiler rejects them.
/// Listed so that a change in either direction is noticed.
#[test]
fn semantic_errors_are_left_to_the_compiler() {
    let validator = jsonschema::draft202012::new(&timeline_schema()).expect("schema compiles");
    for (name, doc) in [
        (
            "unknown anchor target",
            set(
                base(),
                "/tracks/0/sequence/2/start_relative",
                json!({"after_cue": "zz"}),
            ),
        ),
        (
            "duration string beyond 24 h",
            set(base(), "/tracks/1/start_at_ms", json!("25h")),
        ),
        (
            "unknown output extension",
            set(base(), "/output", json!({"path": "out.aiff"})),
        ),
        (
            "mp3 at 96 kHz",
            set(
                base(),
                "/output",
                json!({"format": "mp3", "sample_rate": 96000}),
            ),
        ),
    ] {
        assert!(validator.is_valid(&doc), "the schema rejects {name}");
        assert!(
            !errors(&compiled(&doc)).is_empty(),
            "the compiler accepts {name}"
        );
    }
}
