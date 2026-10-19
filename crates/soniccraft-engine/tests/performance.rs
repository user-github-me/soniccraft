//! Performance sanity: a one-minute, four-track timeline renders in well under 10 s in the dev profile
//! (opt-level 1 for workspace crates).

mod common;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use common::*;
use soniccraft_engine::RenderOverrides;

const SENTENCES: [&str; 6] = [
    "Welcome back to the show, where we take apart one idea per episode.",
    "Today we look at how declarative timelines turn scripts into finished audio.",
    "The engine lays every cue out on a sample accurate grid.",
    "Music beds duck under speech, and the master is normalized to the target loudness.",
    "Subtitles follow the words, so captions always line up with the voice.",
    "That is all for now, thanks for listening.",
];

fn one_minute_timeline() -> String {
    let mut text = String::from(
        r#"version: "1.0"
title: "Performance sanity"
output:
  format: mp3
  export_subtitles: [vtt, srt]
  export_manifest: true
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    loop: true
    fade_in_ms: 2000
    fade_out_ms: 3000
    ducking: { trigger_tracks: [host, guest] }
  - id: whoosh
    type: sfx
    clips:
      - { source: assets/sfx/whoosh.wav, start_at_ms: 0 }
      - { source: assets/sfx/whoosh.wav, start_at_ms: 30000 }
"#,
    );
    for (track, voice) in [("host", "host"), ("guest", "guest")] {
        let _ = write!(
            text,
            "  - id: {track}\n    type: speech\n    speaker: {voice}\n    provider: mock\n    voice_id: {voice}\n    sequence:\n"
        );
        for (i, sentence) in SENTENCES.iter().enumerate() {
            let _ = writeln!(text, "      - text: \"{sentence}\"");
            if track == "host" && i == 0 {
                continue;
            }
            let _ = writeln!(text, "      - pause: 400ms");
        }
    }
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn one_minute_renders_quickly() {
    let project = Project::new();
    let source = project.timeline("minute.yaml", &one_minute_timeline());
    let engine = project.engine();
    let plan = engine.plan(&source).await;
    assert!(plan.valid, "{:#?}", plan.diagnostics);
    let started = Instant::now();
    let report = engine
        .render(
            &source,
            &RenderOverrides {
                preview: true,
                ..RenderOverrides::default()
            },
        )
        .await
        .unwrap_or_else(|f| panic!("{}", serde_json::to_string_pretty(&f).unwrap()));
    let elapsed = started.elapsed();
    assert!(
        report.output.duration_ms >= 60_000.0,
        "the timeline is at least a minute long: {} ms",
        report.output.duration_ms
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "render took {elapsed:?}: {:?}",
        report.timings_ms
    );
    eprintln!(
        "rendered {:.1} s of audio in {elapsed:?}: {:?}",
        report.output.duration_ms / 1000.0,
        report.timings_ms
    );
    // Turn-taking speakers never overlap after the push policy.
    let cues: Vec<_> = report
        .items
        .iter()
        .filter(|i| i.kind == soniccraft_engine::ItemKind::Cue)
        .collect();
    for pair in cues.windows(2) {
        assert!(
            pair[1].start_ms + 20.0 >= pair[0].end_ms,
            "{:?} overlaps {:?}",
            pair[0],
            pair[1]
        );
    }
}
