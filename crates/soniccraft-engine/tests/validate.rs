//! Environment diagnostics of `validate` (and their effect on `plan` and `render`).

mod common;

use std::path::Path;

use common::*;
use soniccraft_core::{Diagnostic, LowerOptions, SourceFormat};
use soniccraft_dsp::AudioBuffer;
use soniccraft_engine::{
    DurationSource, Engine, EngineOptions, ItemKind, RenderOverrides, Severity, ValidateOptions,
    ValidationReport,
};
use soniccraft_tts::SynthesisRequest;

fn find<'a>(report: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    report
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("no `{code}` in {report:#?}"))
}

async fn validate(engine: &Engine, source: &soniccraft_engine::TimelineSource) -> ValidationReport {
    engine.validate(source, &ValidateOptions::default()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_asset_gets_a_did_you_mean_hint_and_stops_renders_before_synthesis() {
    let project = Project::new();
    let text = README.replace("assets/sfx/whoosh.wav", "assets/sfx/whosh.wav");
    let source = project.timeline("briefing.yaml", &text);
    let engine = project.engine();

    let report = validate(&engine, &source).await;
    assert!(!report.valid);
    let d = find(&report.diagnostics, "asset_not_found");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(d.path.as_deref(), Some("tracks[3].source"));
    assert!(d.line.is_some() && d.column.is_some(), "{d:?}");
    assert_eq!(
        d.hint.as_deref(),
        Some("Did you mean `assets/sfx/whoosh.wav`?")
    );
    assert_eq!(d.related, ["sfx_impact"]);
    assert!(report.summary.is_some());

    let plan = engine.plan(&source).await;
    assert!(!plan.valid);
    find(&plan.diagnostics, "asset_not_found");
    assert!(
        plan.items.iter().any(|i| i.id == "sfx_impact"),
        "still laid out"
    );

    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    find(&failure.diagnostics, "asset_not_found");
    assert!(!failure.cancelled);
    let cached = soniccraft_tts::cache_stats(&project.cache_dir()).unwrap();
    assert_eq!(cached.entries, 0, "no synthesis before the asset check");
    assert!(!project.join("briefing.wav").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_directory_and_unreadable_assets() {
    let project = Project::new();
    std::fs::write(
        project.join("assets/sfx/broken.wav"),
        b"definitely not audio",
    )
    .unwrap();
    let text = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Hello.", provider: mock }
  - id: fx
    type: sfx
    clips:
      - { id: broken, source: assets/sfx/broken.wav }
      - { id: lost, source: asset/sfx/whoosh.wav }
"#;
    let source = project.timeline("t.yaml", text);
    let report = validate(&project.engine(), &source).await;
    let unreadable = find(&report.diagnostics, "asset_unreadable");
    assert_eq!(
        unreadable.path.as_deref(),
        Some("tracks[1].clips[0].source")
    );
    assert!(unreadable.message.contains("assets/sfx/broken.wav"));
    let missing = find(&report.diagnostics, "asset_not_found");
    assert_eq!(missing.path.as_deref(), Some("tracks[1].clips[1].source"));
    assert_eq!(
        missing.hint.as_deref(),
        Some("The directory `asset` does not exist; did you mean `assets/sfx/whoosh.wav`?")
    );
}

/// The request the engine builds for every cue (to seed the TTS cache like a previous render would).
fn requests(text: &str, base: &Path) -> Vec<SynthesisRequest> {
    let compiled = soniccraft_core::compile(
        text,
        SourceFormat::Yaml,
        &LowerOptions {
            base_dir: base.to_path_buf(),
            ..LowerOptions::default()
        },
    );
    let program = compiled.program.expect("valid timeline");
    program
        .cues()
        .map(|(_, node, cue)| SynthesisRequest {
            cue_id: node.id.clone(),
            provider: program.providers[&cue.voice.provider].clone(),
            voice: cue.voice.clone(),
            text: cue.text.clone(),
            base_dir: base.to_path_buf(),
        })
        .collect()
}

/// Store `audio` as the cache entry of `request` (DESIGN.md §5.1 layout: `<key>.wav` + `<key>.json`).
fn seed_cache(cache: &Path, request: &SynthesisRequest, audio: &AudioBuffer) {
    let key = soniccraft_tts::cache_key(request).unwrap();
    std::fs::create_dir_all(cache).unwrap();
    soniccraft_dsp::wav::write_f32(&cache.join(format!("{key}.wav")), audio).unwrap();
    let meta = serde_json::json!({
        "cache_version": soniccraft_tts::CACHE_VERSION,
        "key": key,
        "provider_type": request.provider.provider_type.as_str(),
        "voice_id": request.voice.voice_id,
        "text": request.text,
        "sample_rate": audio.sample_rate,
        "channels": audio.num_channels(),
        "frames": audio.frames(),
        "duration_secs": audio.duration_secs(),
        "words": null,
    });
    std::fs::write(
        cache.join(format!("{key}.json")),
        serde_json::to_vec(&meta).unwrap(),
    )
    .unwrap();
}

/// 1.2 s of "speech" (a tone with short silences around it) at 24 kHz.
fn fake_speech() -> AudioBuffer {
    let rate = 24_000;
    let samples = (0..(1.2 * f64::from(rate)) as usize)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            if (0.1..1.1).contains(&t) {
                (0.3 * (2.0 * std::f64::consts::PI * 180.0 * t).sin()) as f32
            } else {
                0.0
            }
        })
        .collect();
    AudioBuffer::mono(rate, samples)
}

const ELEVENLABS: &str = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - id: hello
        text: "Hello from ElevenLabs."
        provider: elevenlabs
        voice_id: "21m00Tcm4TlvDq8ikWAM"
      - id: bye
        text: "And goodbye."
        provider: elevenlabs
        voice_id: "21m00Tcm4TlvDq8ikWAM"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn missing_credentials_are_an_error_unless_every_cue_is_cached() {
    let project = Project::new();
    let source = project.timeline("eleven.yaml", ELEVENLABS);
    // The hermetic environment has no ELEVENLABS_API_KEY.
    let engine = project.engine();

    let report = validate(&engine, &source).await;
    assert!(!report.valid);
    let d = find(&report.diagnostics, "missing_credentials");
    assert_eq!(d.severity, Severity::Error);
    assert_eq!(d.path.as_deref(), Some("tracks[0].sequence[0]"));
    assert_eq!(d.related, ["hello", "bye"]);
    assert!(
        d.message.contains("ELEVENLABS_API_KEY") && d.message.contains("2 of 2"),
        "{}",
        d.message
    );
    assert!(
        d.hint
            .as_deref()
            .unwrap()
            .contains("export ELEVENLABS_API_KEY=")
    );

    // One cue cached: still an error (the other one needs the API).
    let requests = requests(ELEVENLABS, project.path());
    seed_cache(&project.cache_dir(), &requests[0], &fake_speech());
    let partial = validate(&engine, &source).await;
    let d = find(&partial.diagnostics, "missing_credentials");
    assert_eq!(d.severity, Severity::Error);
    assert!(d.message.contains("1 of 2"), "{}", d.message);
    assert_eq!(partial.summary.as_ref().unwrap().cached_cues, 1);

    // Every cue cached: a warning, and the render needs no API call at all.
    seed_cache(&project.cache_dir(), &requests[1], &fake_speech());
    let cached = validate(&engine, &source).await;
    assert!(cached.valid, "{:#?}", cached.diagnostics);
    assert_eq!(
        find(&cached.diagnostics, "missing_credentials").severity,
        Severity::Warning
    );
    assert_eq!(cached.summary.as_ref().unwrap().cached_cues, 2);

    let plan = engine.plan(&source).await;
    assert!(plan.valid && !plan.estimated);
    assert!(
        plan.items
            .iter()
            .filter(|i| i.kind == ItemKind::Cue)
            .all(|i| i.duration_source == DurationSource::Cache)
    );
    assert_eq!(plan.synthesis.characters_to_synthesize, 0);

    let render = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap();
    assert_eq!(render.synthesis.cache_hits, 2);
    assert!(render.synthesis.providers_called.is_empty());
    assert_eq!(
        find(&render.diagnostics, "missing_credentials").severity,
        Severity::Warning
    );
    for (planned, rendered) in plan.items.iter().zip(&render.items) {
        assert_eq!(
            (planned.start_ms, planned.end_ms),
            (rendered.start_ms, rendered.end_ms)
        );
    }

    // `--no-cache` makes the cached cues irrelevant again.
    let no_cache = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        read_cache: false,
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]));
    let report = validate(&no_cache, &source).await;
    assert_eq!(
        find(&report.diagnostics, "missing_credentials").severity,
        Severity::Error
    );

    // With a key the provider is ready.
    let keyed = Engine::new(EngineOptions {
        cache_dir: Some(project.join("other-cache")),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[("ELEVENLABS_API_KEY", "test-key")]));
    let report = validate(&keyed, &source).await;
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| d.code != "missing_credentials")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reference_and_provider_errors() {
    let project = Project::new();
    let engine = project.engine();

    // Unknown provider (reported by lowering, with did-you-mean).
    let text = ELEVENLABS.replacen("provider: elevenlabs", "provider: elevenlab", 1);
    let report = validate(&engine, &project.timeline("typo.yaml", &text)).await;
    assert!(!report.valid && report.summary.is_none());
    let d = find(&report.diagnostics, "unknown_provider");
    assert!(d.hint.as_deref().unwrap().contains("`elevenlabs`"), "{d:?}");

    // A command provider whose program does not exist.
    let text = r#"version: "1.0"
providers:
  cmd:
    type: command
    command: ["definitely-not-a-program-7f3a", "{output}"]
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Hi.", provider: cmd, voice_id: v }
"#;
    let report = validate(&engine, &project.timeline("cmd.yaml", text)).await;
    let d = find(&report.diagnostics, "provider_unavailable");
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Error, Some("providers.cmd"))
    );
    assert!(
        d.message.contains("definitely-not-a-program-7f3a"),
        "{}",
        d.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn text_limits_ffmpeg_notes_and_muted_triggers() {
    let project = Project::new();
    let long = "word ".repeat(900); // 4500 characters > OpenAI's 4096
    let text = format!(
        r#"version: "1.0"
output: {{ format: ogg }}
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    ducking: {{ trigger_tracks: [voice] }}
  - id: voice
    type: speech
    mute: true
    sequence:
      - id: long
        text: "{long}"
        provider: openai
        model: tts-1
        settings: {{ instructions: "Speak calmly." }}
"#
    );
    let source = project.timeline("limits.yaml", &text);
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        ffmpeg_path: Some(project.join("no-such-ffmpeg")),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[("OPENAI_API_KEY", "test-key")]));
    let report = validate(&engine, &source).await;

    let d = find(&report.diagnostics, "text_too_long");
    assert_eq!(d.path.as_deref(), Some("tracks[1].sequence[0].text"));
    assert!(d.message.contains("4096"), "{}", d.message);
    assert!(d.hint.as_deref().unwrap().starts_with("Split the cue"));

    let d = find(&report.diagnostics, "format_requires_ffmpeg");
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Error, Some("output.format"))
    );

    let d = find(&report.diagnostics, "muted_trigger");
    assert_eq!(d.severity, Severity::Info);
    assert_eq!(d.path.as_deref(), Some("tracks[0].ducking.trigger_tracks"));
    assert_eq!(d.related, ["voice"]);

    let d = find(&report.diagnostics, "setting_ignored");
    assert_eq!(d.severity, Severity::Info);
    assert!(d.message.contains("instructions"), "{}", d.message);
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| d.code != "missing_credentials")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn layout_errors_need_source_lengths() {
    let project = Project::new();
    // bed.wav is 4 s long.
    let text = r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    trim_start_ms: 5000
  - id: voice
    type: speech
    sequence:
      - { text: "Hello.", provider: mock }
"#;
    let report = validate(&project.engine(), &project.timeline("trim.yaml", text)).await;
    assert!(!report.valid);
    let d = find(&report.diagnostics, "invalid_value");
    assert!(d.path.as_deref().unwrap().starts_with("tracks[0]"), "{d:?}");

    let overlap = r#"version: "1.0"
timing: { speech_overlap: error }
tracks:
  - id: a
    type: speech
    sequence:
      - { text: "We both talk at the same time.", provider: mock }
  - id: b
    type: speech
    sequence:
      - { text: "Yes we do.", provider: mock, start_at_ms: 200 }
"#;
    let report = validate(
        &project.engine(),
        &project.timeline("overlap.yaml", overlap),
    )
    .await;
    assert!(!report.valid);
    assert_eq!(
        find(&report.diagnostics, "speech_overlap").severity,
        Severity::Error
    );
    // Only layout errors: notes such as cue_pushed belong to `plan`.
    assert!(report.diagnostics.iter().all(|d| d.code != "cue_pushed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_and_online_checks() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let report = project
        .engine()
        .validate(
            &source,
            &ValidateOptions {
                check_credentials_online: true,
            },
        )
        .await;
    assert!(report.valid, "{:#?}", report.diagnostics);
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let summary = report.summary.unwrap();
    assert_eq!(
        (summary.tracks, summary.speech_cues, summary.clips),
        (4, 3, 2)
    );
    assert_eq!(summary.speakers, ["narrator", "guest"]);
    assert_eq!(summary.providers, ["mock"]);
    assert_eq!(summary.characters, 276);
    assert_eq!(
        (summary.cached_cues, summary.output_format.as_str()),
        (0, "wav")
    );
    assert_eq!((summary.sample_rate, summary.target_lufs), (48_000, -16.0));

    let broken = project.timeline("broken.yaml", "version: \"1.0\"\ntracks: [\n");
    let report = project
        .engine()
        .validate(&broken, &ValidateOptions::default())
        .await;
    assert!(!report.valid && report.summary.is_none());
    assert_eq!(report.diagnostics[0].code, "yaml_syntax");
}

/// mp3 needs ffmpeg only in builds without the native LAME encoder (`mp3` feature, DESIGN.md §11).
#[tokio::test(flavor = "multi_thread")]
async fn mp3_needs_ffmpeg_only_without_the_native_encoder() {
    let project = Project::new();
    let text = README.replace("format: wav", "format: mp3");
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        ffmpeg_path: Some(project.join("no-such-ffmpeg")),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]));
    let report = validate(&engine, &project.timeline("mp3.yaml", &text)).await;
    let flagged = report
        .diagnostics
        .iter()
        .find(|d| d.code == "format_requires_ffmpeg");
    assert_eq!(
        flagged.is_some(),
        !cfg!(feature = "mp3"),
        "{:#?}",
        report.diagnostics
    );
    if let Some(d) = flagged {
        // The alternatives never name the format that needs ffmpeg.
        assert_eq!(
            d.hint.as_deref(),
            Some(
                "Install ffmpeg (e.g. `brew install ffmpeg` or `apt install ffmpeg`), or use `format: wav` or \
                 `format: flac`"
            )
        );
        assert_eq!(d.path.as_deref(), Some("output.format"));
    }
}
