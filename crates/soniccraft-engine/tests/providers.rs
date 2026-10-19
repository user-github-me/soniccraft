//! Provider behaviour seen through the engine: dry runs never synthesize, real local voices render, the
//! providers report and voice listing.

mod common;

use common::*;
use soniccraft_engine::{DurationSource, ItemKind, RenderOverrides, Severity, ValidateOptions};

/// A `command` provider that leaves a marker file and fails whenever it is invoked.
fn tripwire_timeline(marker: &std::path::Path) -> String {
    format!(
        r#"version: "1.0"
providers:
  tripwire:
    type: command
    command:
      - /bin/sh
      - -c
      - 'echo invoked >> "$0"; echo "tripwire fired" >&2; exit 9'
      - "{}"
tracks:
  - id: voice
    type: speech
    sequence:
      - id: guarded
        text: "A dry run must never synthesize this sentence."
        provider: tripwire
        voice_id: any
"#,
        marker.display()
    )
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn validate_and_plan_never_call_providers() {
    let project = Project::new();
    let marker = project.join("invoked.txt");
    let source = project.timeline("tripwire.yaml", &tripwire_timeline(&marker));
    let engine = project.engine();

    let report = engine
        .validate(
            &source,
            &ValidateOptions {
                check_credentials_online: true,
            },
        )
        .await;
    assert!(report.valid, "{:#?}", report.diagnostics);
    let plan = engine.plan(&source).await;
    assert!(plan.valid && plan.estimated, "{:#?}", plan.diagnostics);
    let cue = plan.items.iter().find(|i| i.kind == ItemKind::Cue).unwrap();
    assert_eq!(cue.duration_source, DurationSource::Estimate);
    assert!(cue.duration_ms > 1000.0, "{cue:?}");
    assert_eq!(
        plan.synthesis.by_provider["tripwire"].provider_type,
        "command"
    );
    assert!(plan.synthesis.characters_to_synthesize > 0);
    assert!(!marker.exists(), "validate/plan invoked the provider");

    // The tripwire is real: a render invokes it and reports the failure precisely.
    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    assert!(marker.exists());
    let d = failure
        .diagnostics
        .iter()
        .find(|d| d.code == "synthesis_failed")
        .unwrap_or_else(|| panic!("{failure:#?}"));
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Error, Some("tracks[0].sequence[0]"))
    );
    assert!(d.message.contains("guarded") && d.hint.is_some(), "{d:?}");
    assert!(
        failure.message.contains("synthesis failed for 1 of 1 cues"),
        "{}",
        failure.message
    );
    assert!(!project.join("tripwire.wav").exists());
}

/// Whether the macOS voice `Samantha` (the provider default) is installed.
#[cfg(target_os = "macos")]
fn samantha_installed() -> bool {
    let listing = std::process::Command::new("say").args(["-v", "?"]).output();
    let installed = listing.is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.starts_with("Samantha"))
    });
    if !installed {
        eprintln!("skipping: the macOS voice Samantha is not installed");
    }
    installed
}

/// Real speech from the system voice: audible, mastered, captioned with estimated word timings.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn macos_say_renders_real_speech() {
    if !samantha_installed() {
        return;
    }
    let project = Project::new();
    let text = r#"version: "1.0"
output: { format: wav, export_subtitles: [json] }
tracks:
  - id: voice
    type: speech
    sequence:
      - id: hello
        text: "Hello from the macOS system voice. This sentence is real speech."
        provider: macos_say
        voice_id: Samantha
"#;
    let source = project.timeline("say.yaml", text);
    let report = project
        .engine()
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_or_else(|f| panic!("{}", serde_json::to_string_pretty(&f).unwrap()));
    let cue = report.items.iter().find(|i| i.id == "hello").unwrap();
    assert!((1500.0..10_000.0).contains(&cue.duration_ms), "{cue:?}");
    assert_eq!(report.synthesis.providers_called, ["macos_say"]);
    let audio = decode(&project.join("say.wav"));
    assert!((lufs(&audio) + 16.0).abs() <= 0.3);
    assert!(true_peak_dbtp(&audio) <= -1.0);
    // Speech has pauses between words: several distinct speech regions.
    let regions = soniccraft_dsp::analysis::speech_regions(&audio.to_mono(), -45.0, 2_400);
    assert!(regions.len() >= 2, "{regions:?}");
    // `say` returns no word timings: they are estimated (once per provider).
    let estimated: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "alignment_estimated")
        .collect();
    assert_eq!(estimated.len(), 1);
    let words = json(&project.join("say.words.json"));
    let words = words["words"].as_array().unwrap();
    assert_eq!(words.len(), 11);
    assert_eq!(words[0]["text"], "Hello");
    let last_end = words.last().unwrap()["end_ms"].as_f64().unwrap();
    assert!(last_end <= cue.end_ms + 0.05 && last_end > cue.start_ms + 1000.0);

    // The second render is served from the cache.
    let again = project
        .engine()
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap();
    assert_eq!(again.synthesis.cache_hits, 1);
    assert!(again.synthesis.providers_called.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn providers_report_and_voice_listing() {
    let project = Project::new();
    let engine = project.engine();
    let report = engine.providers();
    assert_eq!(report.providers.len(), 8);
    let mock = report.providers.iter().find(|p| p.name == "mock").unwrap();
    assert!(mock.available && mock.word_timestamps && mock.local);
    let say = report
        .providers
        .iter()
        .find(|p| p.name == "macos_say")
        .unwrap();
    assert_eq!(say.available, cfg!(target_os = "macos"), "{say:?}");
    assert_eq!(say.aliases, ["say", "system"]);
    let openai = report
        .providers
        .iter()
        .find(|p| p.name == "openai")
        .unwrap();
    assert!(!openai.available && openai.status.contains("OPENAI_API_KEY"));
    let voices = engine.voices("mock").await.unwrap();
    assert_eq!(voices.provider, "mock");
    assert!(!voices.voices.is_empty());
    #[cfg(target_os = "macos")]
    if samantha_installed() {
        let say = engine.voices("say").await.unwrap();
        assert_eq!(say.provider, "macos_say");
        assert!(
            say.voices.iter().any(|v| v.id.starts_with("Samantha")),
            "{:?}",
            say.voices
        );
    }
}
