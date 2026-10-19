//! Positive lowering tests: the IR produced for valid timelines.

mod common;

use common::*;
use serde_json::json;
use soniccraft_core::ir::{
    AnchorMode, AnchorTarget, ClipLength, DuckingMode, FadeCurve, LoudnessPreset, NodeKind,
    OutputFormat, OverlapPolicy, ProviderType, StartRule, SubtitleFormat, TrackKind,
};
use soniccraft_core::{LowerOptions, Program, SourceFormat, compile};

fn program(text: &str) -> Program {
    let c = yaml(text);
    assert_clean(&c);
    c.program.expect("program")
}

#[test]
fn readme_example_lowers_exactly() {
    let options = LowerOptions {
        base_dir: "/project".into(),
        ..LowerOptions::default()
    };
    let c = compile(
        &read_fixture("valid/readme.yaml"),
        SourceFormat::Yaml,
        &options,
    );
    assert!(
        c.diagnostics.is_empty(),
        "the README example must compile cleanly:\n{}",
        dump(&c)
    );
    let p = c.program.expect("program");

    assert_eq!(p.version, "1.0");
    assert_eq!(p.output.format, OutputFormat::Mp3);
    assert!(p.output.format_explicit);
    assert_eq!(p.output.sample_rate, 48_000);
    assert_eq!(p.output.bitrate_bps, Some(320_000));
    assert_eq!(p.output.target_lufs, -16.0);
    assert_eq!(p.output.true_peak_db, -1.0);
    assert_eq!(
        p.output.subtitle_formats,
        [SubtitleFormat::Vtt, SubtitleFormat::Srt]
    );
    assert!(p.subtitles.speaker_labels, "narrator + guest → labels on");
    assert_eq!(p.timing.speech_overlap, OverlapPolicy::Push);

    let ids: Vec<&str> = p.tracks.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "ambient_bed",
            "voice_primary",
            "voice_secondary",
            "sfx_impact"
        ]
    );
    let kinds: Vec<TrackKind> = p.tracks.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        [
            TrackKind::Music,
            TrackKind::Speech,
            TrackKind::Speech,
            TrackKind::Sfx
        ]
    );

    // ambient_bed: shorthand looping clip, ducked by both voices.
    let bed = &p.tracks[0];
    assert_eq!(bed.gain, 0.65);
    assert_eq!(bed.normalize_lufs, Some(-20.0));
    let ducking = bed.ducking.as_ref().expect("ducking");
    assert_eq!(ducking.triggers, [1, 2]);
    assert_eq!(ducking.reduction_db, 18.0);
    assert_eq!(
        (ducking.attack_ms, ducking.release_ms, ducking.hold_ms),
        (80.0, 350.0, 250.0)
    );
    assert_eq!(ducking.lookahead_ms, 80.0, "lookahead defaults to attack");
    assert_eq!(ducking.threshold_db, -40.0);
    assert_eq!(ducking.mode, DuckingMode::Sidechain);
    let bed_clip = p.nodes[0].as_clip().expect("clip");
    assert_eq!(p.nodes[0].id, "ambient_bed");
    assert_eq!(p.nodes[0].path, "tracks[0]");
    assert!(bed_clip.shorthand && bed_clip.looped);
    assert_eq!(bed_clip.length, ClipLength::Natural);
    assert_eq!(
        (bed_clip.fade_in_ms, bed_clip.fade_out_ms),
        (1200.0, 2500.0)
    );
    assert_eq!(bed_clip.fade_curve, FadeCurve::EqualPower);
    assert_eq!(
        bed_clip.source_path,
        std::path::Path::new("/project/assets/audio/synthwave_loop.wav")
    );

    // voice_primary: cue_01, an automatic pause, cue_03 (sequential, so the layout can push it).
    let primary = &p.tracks[1];
    assert_eq!(primary.pan, -0.15);
    assert_eq!(
        primary
            .trim_silence
            .as_ref()
            .map(|t| (t.threshold_db, t.padding_ms)),
        Some((-50.0, 30.0))
    );
    let node_ids: Vec<&str> = primary
        .nodes
        .iter()
        .map(|&n| p.nodes[n].id.as_str())
        .collect();
    assert_eq!(node_ids, ["cue_01", "voice_primary.pause1", "cue_03"]);
    assert!(matches!(p.nodes[2].kind, NodeKind::Pause { duration_ms } if duration_ms == 350.0));
    assert!(p.nodes[2].auto_id);
    assert_eq!(p.nodes[3].start, StartRule::Sequential);
    let cue_01 = p.nodes[1].as_cue().expect("cue");
    assert_eq!(cue_01.voice.provider, "elevenlabs");
    assert_eq!(cue_01.voice.provider_type, ProviderType::Elevenlabs);
    assert_eq!(
        cue_01.voice.voice_id.as_deref(),
        Some("21m00Tcm4TlvDq8ikWAM")
    );
    assert_eq!(
        cue_01.voice.model.as_deref(),
        Some("eleven_multilingual_v2")
    );
    assert_eq!(cue_01.speaker.as_deref(), Some("narrator"));
    assert_eq!(cue_01.speaker_label, "narrator");
    assert_eq!(cue_01.gain, 1.0);

    // voice_secondary: cue_02 starts 150 ms after cue_01 ends, voiced by Kokoro via its alias.
    let cue_02 = &p.nodes[4];
    let StartRule::Relative(rule) = &cue_02.start else {
        panic!("relative start")
    };
    assert_eq!(rule.target, AnchorTarget::Node(1));
    assert_eq!(rule.target_id, "cue_01");
    assert_eq!(rule.mode, AnchorMode::After);
    assert_eq!(rule.offset_ms, 150.0);
    let voice = &cue_02.as_cue().expect("cue").voice;
    assert_eq!(
        (voice.provider.as_str(), voice.provider_type),
        ("local_kokoro", ProviderType::Kokoro)
    );
    assert_eq!(voice.voice_id.as_deref(), Some("am_adam"));

    // sfx_impact: absolute track start, no source normalization by default.
    let sfx = &p.tracks[3];
    assert_eq!(sfx.start, StartRule::Absolute { ms: 0.0 });
    assert_eq!(sfx.normalize_lufs, None);
    assert_eq!(sfx.gain, 0.8);

    // Providers: the referenced built-ins in first-use order, with defaults filled.
    let names: Vec<&str> = p.providers.keys().map(String::as_str).collect();
    assert_eq!(names, ["elevenlabs", "local_kokoro"]);
    let eleven = &p.providers["elevenlabs"];
    assert!(!eleven.configured);
    assert_eq!(eleven.api_key_env.as_deref(), Some("ELEVENLABS_API_KEY"));
    assert_eq!(
        eleven.base_url.as_deref(),
        Some("https://api.elevenlabs.io")
    );
    assert_eq!((eleven.max_concurrency, eleven.timeout_ms), (2, 120_000));
    assert_eq!(p.providers["local_kokoro"].max_concurrency, 1);

    // The shared id namespace: tracks, cues and clips (pauses are not anchors; the shorthand clip is its track).
    let id_keys: Vec<&str> = p.ids.keys().map(String::as_str).collect();
    assert_eq!(
        id_keys,
        [
            "ambient_bed",
            "voice_primary",
            "cue_01",
            "cue_03",
            "voice_secondary",
            "cue_02",
            "sfx_impact"
        ]
    );
    assert_eq!(p.ids["ambient_bed"], AnchorTarget::Track(0));
    assert_eq!(p.ids["cue_03"], AnchorTarget::Node(3));
}

#[test]
fn voice_layers_merge_in_priority_order() {
    let p = program(
        r#"version: "1.0"
providers:
  studio:
    type: openai
    model: tts-1
    settings: {speed: 1.0, instance_only: true}
defaults:
  provider: studio
  language: en
  settings: {speed: 1.1, defaults_only: 1}
  gain_db: -2
speakers:
  narrator:
    voice_id: nova
    label: Narrator
    settings: {speed: 1.2, narrator_only: 1}
    gain_db: -6
  guest:
    voice_id: echo
    model: gpt-4o-mini-tts
    settings: {speed: 1.3}
tracks:
  - id: voice
    type: speech
    speaker: narrator
    model: tts-1-hd
    settings: {track_only: 1}
    sequence:
      - id: plain
        text: "Uses the track speaker."
        volume: 0.5
      - id: guest_cue
        text: "An explicit cue speaker overrides the track's."
        speaker: guest
        settings: {speed: 0.9}
      - id: explicit
        text: "Cue fields win."
        voice_id: onyx
        language: de
"#,
    );
    let cue = |id: &str| {
        p.nodes
            .iter()
            .find(|n| n.id == id)
            .and_then(|n| n.as_cue())
            .expect(id)
    };

    let plain = cue("plain");
    assert_eq!(plain.voice.provider, "studio");
    assert_eq!(plain.voice.voice_id.as_deref(), Some("nova"));
    assert_eq!(
        plain.voice.model.as_deref(),
        Some("tts-1-hd"),
        "track model beats the instance model"
    );
    assert_eq!(plain.voice.language.as_deref(), Some("en"));
    assert_eq!(
        serde_json::Value::Object(plain.voice.settings.clone()),
        json!({"speed": 1.2, "instance_only": true, "defaults_only": 1, "narrator_only": 1, "track_only": 1})
    );
    assert_eq!(plain.speaker_label, "Narrator");
    // volume 0.5 × 10^(-6/20): the speaker's gain replaces the defaults' gain.
    assert!((plain.gain - 0.5 * 10f64.powf(-6.0 / 20.0)).abs() < 1e-12);

    let guest = cue("guest_cue");
    assert_eq!(guest.speaker.as_deref(), Some("guest"));
    assert_eq!(guest.speaker_label, "guest");
    assert_eq!(guest.voice.voice_id.as_deref(), Some("echo"));
    assert_eq!(
        guest.voice.model.as_deref(),
        Some("gpt-4o-mini-tts"),
        "cue speaker beats track fields"
    );
    assert_eq!(guest.voice.settings["speed"], json!(0.9));
    assert_eq!(
        guest.voice.settings["narrator_only"],
        json!(1),
        "track speaker layer still applies below"
    );
    // guest has no gain_db: the defaults' gain applies.
    assert!((guest.gain - 10f64.powf(-2.0 / 20.0)).abs() < 1e-12);

    let explicit = cue("explicit");
    assert_eq!(explicit.voice.voice_id.as_deref(), Some("onyx"));
    assert_eq!(explicit.voice.language.as_deref(), Some("de"));

    let studio = &p.providers["studio"];
    assert!(studio.configured);
    assert_eq!(studio.provider_type, ProviderType::Openai);
    assert_eq!(studio.model.as_deref(), Some("tts-1"));
    assert_eq!(studio.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    assert_eq!(studio.max_concurrency, 4);
    assert_eq!(p.providers.len(), 1);
}

#[test]
fn configured_instance_fields_are_copied() {
    let c = json(&read_fixture("valid/podcast.json"));
    assert_clean(&c);
    let p = c.program.expect("program");
    let studio = &p.providers["studio"];
    assert_eq!(studio.base_url.as_deref(), Some("http://localhost:8880/v1"));
    assert_eq!(studio.model.as_deref(), Some("kokoro"));
    assert_eq!(studio.max_concurrency, 2);
    assert_eq!(studio.chars_per_second, Some(16.5));
    assert_eq!(studio.words_per_minute, None);
    assert_eq!(p.output.format, OutputFormat::M4a);
    assert_eq!(p.output.bitrate_bps, Some(160_000));
    assert_eq!(p.output.subtitle_formats, [SubtitleFormat::Vtt]);
    let pause = p
        .nodes
        .iter()
        .find(|n| n.id == "narration.pause1")
        .expect("pause");
    assert!(matches!(pause.kind, NodeKind::Pause { duration_ms } if duration_ms == 750.0));
    let second = p
        .nodes
        .iter()
        .find(|n| n.id == "narration.cue2")
        .and_then(|n| n.as_cue())
        .expect("auto id");
    assert_eq!(second.voice.settings["speed"], json!(0.95));
}

#[test]
fn offline_demo_fixture() {
    let c = yaml(&read_fixture("valid/offline_demo.yaml"));
    assert!(c.diagnostics.is_empty(), "{}", dump(&c));
    let p = c.program.expect("program");
    assert_eq!(p.title.as_deref(), Some("Offline demo"));
    assert_eq!(
        p.output.format,
        OutputFormat::Flac,
        "from the output path extension"
    );
    assert!(!p.output.format_explicit);
    assert_eq!(p.output.path.as_deref(), Some("renders/offline-demo.flac"));
    assert_eq!((p.output.target_lufs, p.output.true_peak_db), (-14.0, -1.0));
    assert_eq!(p.output.loudness_preset, LoudnessPreset::Streaming);
    assert_eq!(p.output.bitrate_bps, None);
    assert_eq!(p.output.bit_depth, 16);
    assert_eq!(p.output.tail_ms, Some(1500.0));
    assert_eq!(p.output.subtitle_formats.len(), 4);
    assert!(p.output.export_stems && p.output.export_manifest);
    assert_eq!(p.subtitles.max_chars_per_line, 38);
    assert_eq!(p.timing.min_gap_ms, 300.0);
    assert_eq!(p.mix.reference_lufs, -19.0);

    let bed = &p.tracks[0];
    assert_eq!(
        bed.normalize_lufs,
        Some(-19.0),
        "music normalizes to mix.reference_lufs"
    );
    assert_eq!(
        bed.ducking.as_ref().map(|d| (d.reduction_db, d.mode)),
        Some((12.0, DuckingMode::Cue))
    );
    let intro = p.nodes[bed.nodes[0]].as_clip().expect("clip");
    assert_eq!(intro.length, ClipLength::Duration { ms: 4000.0 });
    let loop_bed = &p.nodes[bed.nodes[1]];
    let clip = loop_bed.as_clip().expect("clip");
    assert!(clip.looped);
    assert_eq!((clip.crossfade_ms, clip.loop_crossfade_ms), (800.0, 50.0));
    assert_eq!(clip.fade_curve, FadeCurve::Exponential);
    let ClipLength::EndRelative(rule) = &clip.length else {
        panic!("end_relative")
    };
    assert_eq!(
        (rule.target_id.as_str(), rule.mode, rule.offset_ms),
        ("outro", AnchorMode::After, 2000.0)
    );

    let host = &p.tracks[1];
    let StartRule::Relative(start) = &host.start else {
        panic!("track start")
    };
    assert_eq!(
        (start.target_id.as_str(), start.mode),
        ("loop_bed", AnchorMode::With)
    );
    assert_eq!(
        host.trim_silence
            .as_ref()
            .map(|t| (t.threshold_db, t.padding_ms)),
        Some((-45.0, 40.0))
    );
    let welcome = p.nodes[host.nodes[0]].as_cue().expect("cue");
    assert_eq!(welcome.voice.provider, "mock");
    assert_eq!(welcome.voice.voice_id.as_deref(), Some("mock-host"));
    assert_eq!(welcome.speaker_label, "Host");

    let answer = p
        .nodes
        .iter()
        .find(|n| n.id == "answer")
        .and_then(|n| n.as_cue())
        .expect("answer");
    assert_eq!(answer.voice.provider, "say");
    assert_eq!(answer.voice.provider_type, ProviderType::MacosSay);
    assert_eq!(
        answer.display_text(),
        "It compiles timelines into mastered audio!"
    );
    assert!((answer.gain - 10f64.powf(-1.5 / 20.0)).abs() < 1e-12);

    let chime = &p.tracks[3];
    assert_eq!(chime.normalize_lufs, Some(-24.0));
    let chime_clip = &p.nodes[chime.nodes[0]];
    assert_eq!(chime_clip.id, "chime");
    let StartRule::Relative(rule) = &chime.start else {
        panic!("chime start")
    };
    assert_eq!((rule.mode, rule.offset_ms), (AnchorMode::Before, -100.0));

    let providers: Vec<&str> = p.providers.keys().map(String::as_str).collect();
    assert_eq!(providers, ["mock", "say"]);
}

#[test]
fn output_resolution_and_overrides() {
    let base = BASE.replace("tracks:", "output: {path: out/episode.ogg}\ntracks:");
    let p = program(&base);
    assert_eq!(
        (p.output.format, p.output.bitrate_bps),
        (OutputFormat::Ogg, Some(160_000))
    );

    // The override path's extension selects the format.
    let options = LowerOptions {
        output_path_override: Some("final.opus".into()),
        ..LowerOptions::default()
    };
    let c = yaml_with(&base, &options);
    assert_clean(&c);
    let p = c.program.expect("program");
    assert_eq!(p.output.format, OutputFormat::Opus);
    assert_eq!(p.output.path.as_deref(), Some("final.opus"));
    assert_eq!(p.output.bitrate_bps, Some(96_000));

    // The format override wins over everything.
    let options = LowerOptions {
        output_path_override: Some("final.opus".into()),
        format_override: Some(OutputFormat::Wav),
        ..LowerOptions::default()
    };
    let c = yaml_with(&base, &options);
    assert_eq!(count(&c, "format_extension_mismatch"), 1);
    let p = c.program.expect("program");
    assert_eq!(p.output.format, OutputFormat::Wav);
    assert!(p.output.format_explicit);

    // Defaults.
    let p = program(BASE);
    assert_eq!(p.output.format, OutputFormat::Wav);
    assert_eq!(
        (p.output.sample_rate, p.output.channels, p.output.bit_depth),
        (48_000, 2, 24)
    );
    assert_eq!(p.output.bitrate_bps, None);
    assert_eq!(p.output.loudness_preset, LoudnessPreset::Podcast);
    assert!(p.output.normalize && p.output.dither);
    assert!(p.output.subtitle_formats.is_empty());
    assert_eq!(
        (p.subtitles.max_chars_per_line, p.subtitles.max_lines),
        (42, 2)
    );
    assert_eq!(
        (p.subtitles.max_duration_ms, p.subtitles.min_duration_ms),
        (6000.0, 800.0)
    );
    assert!(!p.subtitles.speaker_labels, "one speaker label → off");
    assert_eq!(
        (p.timing.min_gap_ms, p.timing.overlap_tolerance_ms),
        (250.0, 20.0)
    );
    assert_eq!(p.mix.reference_lufs, -20.0);

    // Presets and explicit targets.
    let p = program(&BASE.replace(
        "tracks:",
        "output: {loudness_preset: ebu_r128, true_peak_db: -2}\ntracks:",
    ));
    assert_eq!(
        (
            p.output.loudness_preset,
            p.output.target_lufs,
            p.output.true_peak_db
        ),
        (LoudnessPreset::Broadcast, -23.0, -2.0)
    );
    let p = program(&BASE.replace("tracks:", "output: {loudness_preset: atsc}\ntracks:"));
    assert_eq!((p.output.target_lufs, p.output.true_peak_db), (-24.0, -2.0));

    // mp3 bitrates: low sample rates silently cap the default; explicit values are rounded with an info.
    let p = program(&BASE.replace(
        "tracks:",
        "output: {format: mp3, sample_rate: 22050}\ntracks:",
    ));
    assert_eq!(p.output.bitrate_bps, Some(160_000));
    let c = yaml(&BASE.replace("tracks:", "output: {format: mp3, bitrate: 150k}\ntracks:"));
    assert_eq!(count(&c, "bitrate_adjusted"), 1);
    assert_eq!(
        c.program.expect("info only").output.bitrate_bps,
        Some(160_000)
    );
    let c = yaml(&BASE.replace("tracks:", "output: {format: mp3, bitrate: 56k}\ntracks:"));
    assert_eq!(count(&c, "bitrate_adjusted"), 0);
}

#[test]
fn automatic_ids_for_cues_pauses_and_clips() {
    let p = program(
        r#"version: "1.0"
tracks:
  - id: v
    type: speech
    sequence:
      - {text: "a", provider: mock}
      - pause: 1s
      - {id: named, text: "b", provider: mock}
      - {text: "c", provider: mock}
      - pause_duration: 250
  - id: m
    type: music
    clips:
      - source: a.wav
      - {id: second, source: b.wav, start_relative: {after_cue: v.cue3}}
      - source: c.wav
"#,
    );
    let ids: Vec<(&str, bool)> = p.nodes.iter().map(|n| (n.id.as_str(), n.auto_id)).collect();
    assert_eq!(
        ids,
        [
            ("v.cue1", true),
            ("v.pause1", true),
            ("named", false),
            ("v.cue3", true),
            ("v.pause2", true),
            ("m.clip1", true),
            ("second", false),
            ("m.clip3", true)
        ]
    );
    assert!(!p.ids.contains_key("v.pause1"));
    assert_eq!(p.ids["v.cue3"], AnchorTarget::Node(3));
    assert_eq!(p.nodes[6].index_in_track, 1);
    assert_eq!(p.nodes[6].path, "tracks[1].clips[1]");
}

#[test]
fn normalize_trim_and_ducking_defaults() {
    let p = program(
        r#"version: "1.0"
mix: {reference_lufs: -18}
tracks:
  - id: v
    type: speech
    normalize: false
    trim_silence: false
    sequence:
      - {text: "a", provider: mock}
  - id: fx
    type: sfx
    source: hit.wav
    normalize: true
    ducking: {trigger_tracks: [v], reduction_db: 9, attack_ms: 30, lookahead_ms: 10}
  - id: bed
    type: music
    source: bed.wav
    normalize: {target_lufs: -26}
    ducking: {trigger_tracks: [v, v]}
"#,
    );
    assert_eq!(p.tracks[0].normalize_lufs, None);
    assert_eq!(p.tracks[0].trim_silence, None);
    assert_eq!(p.tracks[1].normalize_lufs, Some(-18.0));
    assert_eq!(p.tracks[2].normalize_lufs, Some(-26.0));
    let fx = p.tracks[1].ducking.as_ref().expect("ducking");
    assert_eq!(
        (fx.reduction_db, fx.attack_ms, fx.lookahead_ms),
        (9.0, 30.0, 10.0)
    );
    let bed = p.tracks[2].ducking.as_ref().expect("ducking");
    assert_eq!(bed.triggers, [0], "duplicate triggers are merged");
    assert_eq!(
        (
            bed.reduction_db,
            bed.attack_ms,
            bed.release_ms,
            bed.hold_ms,
            bed.lookahead_ms,
            bed.threshold_db
        ),
        (15.0, 80.0, 400.0, 250.0, 80.0, -40.0)
    );
}

#[test]
fn speaker_labels_switch() {
    let two = r#"version: "1.0"
tracks:
  - id: a
    type: speech
    sequence: [{text: "x", provider: mock}]
  - id: b
    type: speech
    sequence: [{text: "y", provider: mock}]
"#;
    assert!(
        program(two).subtitles.speaker_labels,
        "two track-id labels → on"
    );
    let explicit = two.replace("tracks:", "subtitles: {speaker_labels: false}\ntracks:");
    assert!(!program(&explicit).subtitles.speaker_labels);
    let hidden = two.replace(
        "[{text: \"y\", provider: mock}]",
        "[{text: \"y\", provider: mock, subtitles: false}]",
    );
    assert!(
        !program(&hidden).subtitles.speaker_labels,
        "cues excluded from subtitles do not count"
    );
}

#[test]
fn yaml_and_json_produce_the_same_program() {
    let yaml_text = read_fixture("valid/readme.yaml");
    let value: serde_json::Value = serde_saphyr::from_str(&yaml_text).expect("yaml");
    let json_text = serde_json::to_string_pretty(&value).expect("json");
    let a = yaml(&yaml_text).program.expect("yaml program");
    let b = json(&json_text).program.expect("json program");
    assert_eq!(a, b);
}

#[test]
fn compilation_is_deterministic() {
    let text = read_fixture("valid/offline_demo.yaml");
    let first = yaml(&text);
    for _ in 0..5 {
        let again = yaml(&text);
        assert_eq!(again.diagnostics, first.diagnostics);
        assert_eq!(again.program, first.program);
        assert_eq!(
            serde_json::to_string(&again.program).expect("serialize"),
            serde_json::to_string(&first.program).expect("serialize")
        );
    }
}
