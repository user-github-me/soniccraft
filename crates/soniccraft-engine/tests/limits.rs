//! Dry runs and their limits: exact mock durations, interval-based ducking for very long timelines, the
//! render memory budget, layout errors that depend on estimates, and empty timelines.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use soniccraft_core::Diagnostic;
use soniccraft_engine::{
    DurationSource, Engine, EngineOptions, ItemKind, RenderOverrides, Severity, TimelineSource,
    ValidateOptions,
};

fn find<'a>(report: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    report
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("no `{code}` in {report:#?}"))
}

/// A cold-cache plan of the shipped mock demo is exact: every item starts and ends on the frame the render
/// puts it.
#[tokio::test(flavor = "multi_thread")]
async fn the_mock_demo_plans_exactly_what_it_renders() {
    let demo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/offline-demo/timeline.mock.yaml");
    if !demo.exists() {
        eprintln!("skipping: the examples are not available (packaged crate)");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_cache(&tmp.path().join("cache"));
    let source = TimelineSource::from_path(&demo).unwrap();
    let plan = engine.plan(&source).await;
    assert!(plan.valid, "{:#?}", plan.diagnostics);
    assert!(!plan.estimated);
    assert_eq!(plan.synthesis.cached, 0, "cold cache");
    let report = engine
        .render(
            &source,
            &RenderOverrides {
                output_path: Some(tmp.path().join("demo.wav")),
                export_subtitles: Some(false),
                export_manifest: Some(false),
                ..RenderOverrides::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(plan.duration_ms, report.output.duration_ms);
    assert_eq!(plan.items.len(), report.items.len());
    for (planned, rendered) in plan.items.iter().zip(&report.items) {
        assert_eq!(planned.id, rendered.id);
        assert_eq!(
            (planned.start_ms, planned.end_ms),
            (rendered.start_ms, rendered.end_ms),
            "{}",
            rendered.id
        );
        if planned.kind == ItemKind::Cue {
            assert_eq!(planned.duration_source, DurationSource::Exact);
        }
    }
}

/// A 23-hour timeline with ducking plans and validates in well under a second of work: the regions come
/// from the item intervals, not from per-millisecond arrays.
#[tokio::test(flavor = "multi_thread")]
async fn long_timelines_plan_without_timeline_sized_buffers() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { duration_ms: "23h" }
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    loop: true
    ducking: { trigger_tracks: [voice] }
  - id: fx
    type: sfx
    source: assets/sfx/whoosh.wav
    start_at_ms: "22h"
    ducking: { trigger_tracks: [voice], mode: cue }
  - id: voice
    type: speech
    sequence:
      - { text: "Hello.", provider: mock }
      - pause: "20h"
      - { text: "Still here.", provider: mock }
"#;
    let source = project.timeline("long.yaml", text);
    let engine = project.engine();
    let started = Instant::now();
    let plan = engine.plan(&source).await;
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(plan.duration_ms, 23.0 * 3_600_000.0);
    assert_eq!(plan.ducking.len(), 2);
    for ducking in &plan.ducking {
        assert_eq!(ducking.regions.len(), 2, "{ducking:?}");
        assert!(ducking.regions[1].start_ms > 20.0 * 3_600_000.0);
    }
    // Rendering 23 hours would need far more than the default memory budget: a warning now, an error
    // before any synthesis at render time.
    let warning = find(&report.diagnostics, "timeline_too_long");
    assert_eq!(warning.severity, Severity::Warning);
    assert_eq!(warning.path.as_deref(), Some("output.duration_ms"));
    assert!(warning.message.contains("23.0 h"), "{}", warning.message);
    assert!(report.valid);
}

/// The render memory budget: validate and plan warn, render stops before any TTS request.
#[tokio::test(flavor = "multi_thread")]
async fn renders_over_the_memory_budget_stop_before_synthesis() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[("SONICCRAFT_MAX_RENDER_GB", "0.001")]));
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    let d = find(&report.diagnostics, "timeline_too_long");
    assert_eq!(d.severity, Severity::Warning);
    assert!(report.valid);
    let plan = engine.plan(&source).await;
    assert_eq!(
        find(&plan.diagnostics, "timeline_too_long").severity,
        Severity::Warning
    );
    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    let d = find(&failure.diagnostics, "timeline_too_long");
    assert_eq!(d.severity, Severity::Error);
    assert!(
        d.message.contains("GB of memory") && d.message.contains("0.001 GB"),
        "{}",
        d.message
    );
    assert!(
        d.hint
            .as_deref()
            .unwrap()
            .contains("SONICCRAFT_MAX_RENDER_GB="),
        "{d:?}"
    );
    assert!(
        failure.message.contains("nothing was synthesized"),
        "{}",
        failure.message
    );
    assert_eq!(
        soniccraft_tts::cache_stats(&project.cache_dir())
            .unwrap()
            .entries,
        0
    );
    assert!(!project.join("briefing.wav").exists());

    // A setting that is not a number is reported and the default applies.
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[("SONICCRAFT_MAX_RENDER_GB", "lots")]));
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    let d = find(&report.diagnostics, "invalid_value");
    assert_eq!(d.severity, Severity::Warning);
    assert!(
        d.message.contains("SONICCRAFT_MAX_RENDER_GB"),
        "{}",
        d.message
    );
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| d.code != "timeline_too_long")
    );
}

/// A `command` provider that leaves a marker file whenever it runs (its lengths are estimates).
fn command_provider(marker: &Path) -> String {
    format!(
        r#"providers:
  cmd:
    type: command
    command: ["/bin/sh", "-c", "echo ran >> \"$0\"; exit 3", "{}"]
"#,
        marker.display()
    )
}

/// Layout errors that depend on estimated speech lengths are warnings in dry runs (with a note), while
/// errors that do not are errors, and stop a render before any synthesis.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn estimate_dependent_layout_errors() {
    let project = Project::new();
    let marker = project.join("ran.txt");
    // Overlap under the `error` policy that depends on the estimated length of `long`.
    let overlap = format!(
        r#"version: "1.0"
timing: {{ speech_overlap: error }}
{}tracks:
  - id: a
    type: speech
    sequence:
      - {{ id: long, text: "This sentence takes a few seconds to say out loud.", provider: cmd, voice_id: x }}
  - id: b
    type: speech
    sequence:
      - {{ id: late, text: "Hi.", provider: mock, start_at_ms: 1000 }}
"#,
        command_provider(&marker)
    );
    let source = project.timeline("overlap.yaml", &overlap);
    let engine = project.engine();
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    let d = find(&report.diagnostics, "speech_overlap");
    assert_eq!(d.severity, Severity::Warning);
    assert!(
        d.message.ends_with(
            "(based on estimated speech durations; the render checks again with exact durations)"
        ),
        "{}",
        d.message
    );
    assert!(report.valid, "{:#?}", report.diagnostics);
    let plan = engine.plan(&source).await;
    assert_eq!(
        find(&plan.diagnostics, "speech_overlap").severity,
        Severity::Warning
    );
    assert!(plan.valid);

    // A trim past the end of a source is wrong whatever the speech lengths: an error in validate, and a
    // render stops before calling the provider.
    let trim = format!(
        r#"version: "1.0"
{}tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    trim_start_ms: 5000
  - id: voice
    type: speech
    sequence:
      - {{ text: "Hello.", provider: cmd, voice_id: x }}
"#,
        command_provider(&marker)
    );
    let source = project.timeline("trim.yaml", &trim);
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    assert_eq!(
        find(&report.diagnostics, "invalid_value").severity,
        Severity::Error
    );
    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    let d = find(&failure.diagnostics, "invalid_value");
    assert!(d.path.as_deref().unwrap().starts_with("tracks[0]"), "{d:?}");
    assert!(
        failure.message.contains("nothing was synthesized"),
        "{}",
        failure.message
    );
    assert!(!marker.exists(), "the provider was called");
}

/// An empty timeline is invalid in validate, plan and render alike, with one diagnostic and a hint that
/// fixes it.
#[tokio::test(flavor = "multi_thread")]
async fn empty_timelines_are_invalid_everywhere() {
    let project = Project::new();
    let text = r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    loop: true
"#;
    let source = project.timeline("bo1.yaml", text);
    let engine = project.engine();
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    assert!(!report.valid);
    let d = find(&report.diagnostics, "invalid_value");
    assert!(d.message.contains("empty"), "{}", d.message);
    assert!(d.hint.as_deref().unwrap().contains("fade_out_ms"), "{d:?}");
    let plan = engine.plan(&source).await;
    assert!(!plan.valid);
    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    let empty: Vec<&Diagnostic> = failure
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("empty"))
        .collect();
    assert_eq!(empty.len(), 1, "{:#?}", failure.diagnostics);
    assert_eq!(empty[0].message, d.message);
}

/// The documented "bounded bed" pattern (review-timing cy2): the bed's end waits for the last line, whose
/// track starts with the bed. Start and end are separate events, so this renders without a cycle, and so
/// does core's offline demo fixture (an intro clip, a looped bed anchored after the outro, a track anchored
/// to a clip).
#[tokio::test(flavor = "multi_thread")]
async fn bounded_beds_lead_into_the_voice() {
    let project = Project::new();
    let cy2 = r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    loop: true
    fade_in_ms: 1200
    fade_out_ms: 3000
    end_relative: { after_cue: last_line, offset_ms: 3000 }
  - id: narration
    type: speech
    sequence:
      - { id: first_line, text: "Welcome to the show.", provider: mock, start_relative: { with_cue: bed, offset_ms: 2000 } }
      - { id: last_line, text: "Thanks for listening.", provider: mock }
"#;
    let engine = project.engine();
    let report = engine
        .render(
            &project.timeline("cy2.yaml", cy2),
            &RenderOverrides::default(),
        )
        .await
        .unwrap_or_else(|f| panic!("{f:#?}"));
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| d.code != "dependency_cycle")
    );
    let last = report.items.iter().find(|i| i.id == "last_line").unwrap();
    assert!((report.output.duration_ms - (last.end_ms + 3000.0)).abs() < 0.05);

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../soniccraft-core/tests/fixtures/valid/offline_demo.yaml");
    let Ok(text) = std::fs::read_to_string(&fixture) else {
        eprintln!("skipping the fixture: not available");
        return;
    };
    // Assets the fixture names, and the mock voice instead of macOS `say` (portable).
    project.write_audio("audio/intro.wav", &bed(48_000, 4.0));
    project.write_audio("audio/loop.wav", &bed(48_000, 4.0));
    project.write_audio("sfx/chime.wav", &whoosh(48_000, 0.6));
    let text = text
        .replace("provider: say\n", "provider: mock\n")
        .replace("settings: {rate: 185}", "settings: {wpm: 185}");
    let report = engine
        .render(
            &project.timeline("offline_demo.yaml", &text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap_or_else(|f| panic!("{f:#?}"));
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| d.code != "dependency_cycle"),
        "{:#?}",
        report.diagnostics
    );
    assert!(project.join("renders/offline-demo.flac").exists());
    assert_eq!(report.files.stems.len(), 4);
}

/// Plans of very large timelines keep their diagram to a bounded size.
#[tokio::test(flavor = "multi_thread")]
async fn huge_plans_cut_their_diagram() {
    let project = Project::new();
    let mut text = String::from(
        "version: \"1.0\"\nproviders:\n  cmd:\n    type: command\n    command: [\"/bin/sh\", \"-c\", \"exit 0\", \"{output}\"]\ntracks:\n  - id: voice\n    type: speech\n    sequence:\n",
    );
    for i in 0..4000 {
        text.push_str(&format!(
            "      - {{ text: \"Line number {i} of a very long reading.\", provider: cmd, voice_id: v }}\n"
        ));
    }
    let plan = project
        .engine()
        .plan(&project.timeline("huge.yaml", &text))
        .await;
    assert_eq!(plan.items.len(), 4000);
    assert!(
        plan.diagram.len() <= 256 * 1024 + 200,
        "{}",
        plan.diagram.len()
    );
    assert!(
        plan.diagram.contains("more lines: the diagram is cut"),
        "{}",
        &plan.diagram[plan.diagram.len() - 200..]
    );
}
