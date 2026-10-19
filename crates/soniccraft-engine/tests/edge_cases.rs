//! Edge cases of the render pipeline.

mod common;

use common::*;
use soniccraft_engine::{RenderOverrides, Severity};

fn has(report: &soniccraft_engine::RenderReport, code: &str) -> bool {
    report.diagnostics.iter().any(|d| d.code == code)
}

#[tokio::test(flavor = "multi_thread")]
async fn clip_only_timelines_need_no_tts() {
    let project = Project::new();
    let text = r#"version: "1.0"
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    duration_ms: 3000
    fade_out_ms: 500
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("clip.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    assert_eq!(report.synthesis.cues, 0);
    assert_eq!(report.output.duration_ms, 3000.0);
    let audio = decode(&project.join("clip.wav"));
    assert!((lufs(&audio) + 16.0).abs() < 0.3);
}

#[tokio::test(flavor = "multi_thread")]
async fn muted_mix_is_silent_with_a_warning() {
    let project = Project::new();
    let text = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    mute: true
    sequence:
      - { text: "Nobody hears this.", provider: mock }
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("muted.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    let d = report
        .diagnostics
        .iter()
        .find(|d| d.code == "silent_output")
        .unwrap();
    assert_eq!(d.severity, Severity::Warning);
    assert!(report.loudness.integrated_lufs.is_none() && report.loudness.true_peak_dbtp.is_none());
    assert!(!report.loudness.normalized);
    let audio = decode(&project.join("muted.wav"));
    assert!(audio.peak() == 0.0);
    // The muted cue is still synthesized and laid out (stable timing), but not mixed.
    assert_eq!(report.synthesis.synthesized, 1);
    assert!(report.items[0].duration_ms > 500.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn very_short_output_skips_master_normalization() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { duration_ms: 300 }
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    loop: true
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("short.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    assert_eq!(report.output.duration_ms, 300.0);
    let d = report
        .diagnostics
        .iter()
        .find(|d| d.code == "normalization_skipped" && d.path.as_deref() == Some("output"))
        .unwrap_or_else(|| panic!("{:#?}", report.diagnostics));
    assert_eq!(d.severity, Severity::Info);
    assert!(!report.loudness.normalized);
    assert!(report.loudness.true_peak_dbtp.unwrap() <= -1.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_duration_truncates_content_and_captions() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { duration_ms: 2500, export_subtitles: [json] }
tracks:
  - id: voice
    type: speech
    sequence:
      - id: long
        text: "This sentence is much longer than the two and a half seconds the output allows."
        provider: mock
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("cut.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    assert_eq!(report.output.duration_ms, 2500.0);
    assert!(
        has(&report, "content_truncated"),
        "{:#?}",
        report.diagnostics
    );
    let audio = decode(&project.join("cut.wav"));
    assert_eq!(audio.frames(), 120_000);
    let words = json(&project.join("cut.words.json"));
    for w in words["words"].as_array().unwrap() {
        assert!(w["start_ms"].as_f64().unwrap() < 2500.0, "{w}");
        assert!(w["end_ms"].as_f64().unwrap() <= 2500.0, "{w}");
    }
    for c in words["captions"].as_array().unwrap() {
        assert!(c["end_ms"].as_f64().unwrap() <= 2500.0, "{c}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_normalization_only_the_limiter_runs() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { normalize: false, true_peak_db: -3.0 }
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    normalize: false
    gain_db: 12
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("raw.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    assert!(!report.loudness.normalized);
    assert_eq!(report.loudness.gain_db, 0.0);
    let audio = decode(&project.join("raw.wav"));
    let peak = true_peak_dbtp(&audio);
    assert!(peak <= -3.0, "clip guard at the ceiling: {peak}");
    assert!(report.loudness.limiter_max_reduction_db > 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn caption_overrides_and_excluded_cues() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { export_subtitles: [json, vtt] }
tracks:
  - id: voice
    type: speech
    sequence:
      - id: spoken
        text: "We use S Q L every day."
        caption: "We use SQL every day."
        provider: mock
      - id: hidden
        text: "This cue is not subtitled."
        subtitles: false
        provider: mock
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("captions.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    let words = json(&project.join("captions.words.json"));
    let texts: Vec<&str> = words["words"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["We", "use", "SQL", "every", "day."]);
    assert!(
        words["captions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["cue_id"] == "spoken")
    );
    // Mock timings matched the shared words; `SQL` ← `S Q L` is a partial match.
    let vtt = read(&project.join("captions.vtt"));
    assert!(vtt.contains("We use SQL every day."), "{vtt}");
    assert!(!vtt.contains("not subtitled"));
    assert_eq!(report.items.len(), 2);
}

/// LAME's gapless header needs bigger frames than 32 kbps at 16 kHz mono: the encoder raises the bitrate and
/// the report says so (DESIGN.md §2.3).
#[tokio::test(flavor = "multi_thread")]
async fn raised_mp3_bitrates_are_reported() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { format: mp3, sample_rate: 16000, channels: 1, bitrate: 32k }
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Low bitrate speech.", provider: mock }
"#;
    let engine = project.engine();
    let source = project.timeline("low.yaml", text);
    let report = match engine.render(&source, &RenderOverrides::default()).await {
        Ok(report) => report,
        // Without the native encoder this needs ffmpeg.
        Err(f)
            if f.diagnostics
                .iter()
                .any(|d| d.code == "format_requires_ffmpeg") =>
        {
            return;
        }
        Err(f) => panic!("{f:#?}"),
    };
    assert_eq!(report.output.bitrate_bps, Some(40_000));
    let d = report
        .diagnostics
        .iter()
        .find(|d| d.code == "bitrate_adjusted")
        .unwrap_or_else(|| panic!("{:#?}", report.diagnostics));
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Info, Some("output.bitrate"))
    );
    assert!(
        d.message.contains("40 kbps") && d.message.contains("32 kbps"),
        "{}",
        d.message
    );
    let audio = decode(&project.join("low.mp3"));
    assert_eq!((audio.sample_rate, audio.num_channels()), (16_000, 1));
}

/// ogg, opus and m4a go through ffmpeg (skipped when it is not installed).
#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_formats_render() {
    if soniccraft_dsp::ffmpeg::find_ffmpeg().is_none() {
        eprintln!("skipping: ffmpeg is not installed");
        return;
    }
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let engine = project.engine();
    for (format, rate) in [("ogg", 48_000), ("opus", 48_000), ("m4a", 48_000)] {
        let path = project.join(&format!("out/episode.{format}"));
        let report = engine
            .render(
                &source,
                &RenderOverrides {
                    output_path: Some(path.clone()),
                    format: Some(format.to_string()),
                    ..RenderOverrides::default()
                },
            )
            .await
            .unwrap_or_else(|f| panic!("{format}: {f:#?}"));
        assert_eq!(report.output.format, format);
        assert_eq!(report.output.sample_rate, rate);
        assert!(report.output.bitrate_bps.is_some() && report.output.bytes > 10_000);
        let audio = soniccraft_dsp::decode::decode_file(&path).unwrap();
        let integrated = lufs(&audio);
        assert!(
            (integrated + 16.0).abs() < 0.6,
            "{format}: {integrated} LUFS"
        );
    }
}
