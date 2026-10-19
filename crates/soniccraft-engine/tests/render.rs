//! Render contract tests on the README timeline (mock voice, generated assets).

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use soniccraft_engine::{
    DurationSource, Engine, EngineOptions, ItemKind, ItemReport, ProgressEvent, RenderOverrides,
    RenderReport,
};

fn item<'a>(report: &'a [ItemReport], id: &str) -> &'a ItemReport {
    report
        .iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("no item {id}"))
}

/// Parse `HH:MM:SS.mmm` (or `,` for SRT) into milliseconds.
fn timestamp(s: &str) -> f64 {
    let s = s.replace(',', ".");
    let parts: Vec<&str> = s.split(':').collect();
    assert_eq!(parts.len(), 3, "timestamp {s}");
    let h: f64 = parts[0].parse().unwrap();
    let m: f64 = parts[1].parse().unwrap();
    let sec: f64 = parts[2].parse().unwrap();
    assert!(parts[2].len() == 6, "milliseconds with 3 digits: {s}");
    ((h * 60.0 + m) * 60.0 + sec) * 1000.0
}

/// Cue blocks of a WebVTT or SRT file: (start, end, text lines).
fn cue_blocks(text: &str, srt: bool) -> Vec<(f64, f64, Vec<String>)> {
    let mut blocks = Vec::new();
    let body = if srt {
        text
    } else {
        assert!(text.starts_with("WEBVTT\n"), "WebVTT header");
        &text["WEBVTT\n".len()..]
    };
    for (n, block) in body
        .split("\n\n")
        .map(|b| b.trim_matches('\n'))
        .filter(|b| !b.is_empty())
        .enumerate()
    {
        let mut lines = block.lines();
        if srt {
            assert_eq!(lines.next().unwrap(), (n + 1).to_string(), "SRT index");
        }
        let timing = lines.next().unwrap();
        let (start, end) = timing.split_once(" --> ").expect("timing line");
        let text: Vec<String> = lines.map(str::to_string).collect();
        assert!(
            !text.is_empty() && text.len() <= 2,
            "1–2 text lines: {block}"
        );
        blocks.push((timestamp(start), timestamp(end), text));
    }
    blocks
}

async fn render_readme(project: &Project, engine: &Engine) -> RenderReport {
    let source = project.timeline("briefing.yaml", README);
    let overrides = RenderOverrides {
        export_stems: Some(true),
        preview: true,
        ..RenderOverrides::default()
    };
    match engine.render(&source, &overrides).await {
        Ok(report) => report,
        Err(failure) => panic!(
            "render failed: {}",
            serde_json::to_string_pretty(&failure).unwrap()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn readme_render_meets_the_contract() {
    let project = Project::new();
    let report = render_readme(&project, &project.engine()).await;

    // Output file and loudness, re-measured from the decoded file.
    let out = Path::new(&report.output.path);
    assert!(out.is_absolute() && out.exists(), "{}", report.output.path);
    assert_eq!(out, project.join("briefing.wav"));
    assert_eq!(std::fs::metadata(out).unwrap().len(), report.output.bytes);
    assert_eq!(
        (
            report.output.format.as_str(),
            report.output.sample_rate,
            report.output.channels
        ),
        ("wav", 48_000, 2)
    );
    assert_eq!(report.output.bit_depth, Some(24));
    let audio = decode(out);
    let frames = (report.output.duration_ms * 48.0).round() as usize;
    assert_eq!(audio.frames(), frames);
    let integrated = lufs(&audio);
    assert!(
        (integrated + 16.0).abs() <= 0.3,
        "integrated {integrated} LUFS"
    );
    let peak = true_peak_dbtp(&audio);
    assert!(peak <= -1.0, "true peak {peak} dBTP");
    let loudness = &report.loudness;
    assert!((loudness.integrated_lufs.unwrap() - integrated).abs() < 0.05);
    assert!(loudness.normalized && loudness.iterations >= 1);
    assert_eq!(
        (loudness.target_lufs, loudness.true_peak_target_dbtp),
        (-16.0, -1.0)
    );

    // Geometry: cue_02 150 ms after cue_01; cue_03 pushed to 350 ms after cue_02 (DESIGN.md §3.4).
    let (c1, c2, c3) = (
        item(&report.items, "cue_01"),
        item(&report.items, "cue_02"),
        item(&report.items, "cue_03"),
    );
    assert_eq!(c1.start_ms, 0.0);
    assert!(
        (c2.start_ms - (c1.end_ms + 150.0)).abs() < 0.05,
        "{c1:?} {c2:?}"
    );
    assert!(
        (c3.start_ms - (c2.end_ms + 350.0)).abs() < 0.05,
        "cue_03 pushed: {c3:?}"
    );
    assert!(c3.pushed_from_ms.is_some());
    assert!(report.diagnostics.iter().any(|d| d.code == "cue_pushed"));
    for cue in [c1, c2, c3] {
        assert_eq!(cue.duration_source, DurationSource::Exact);
        assert_eq!(cue.kind, ItemKind::Cue);
    }
    let bed = item(&report.items, "ambient_bed");
    assert!(bed.open_ended && bed.looped);
    assert_eq!(bed.end_ms, report.output.duration_ms);
    assert!(
        (report.output.duration_ms - (c3.end_ms + 2500.0)).abs() < 0.05,
        "tail = bed fade-out"
    );
    let starts: Vec<f64> = report.items.iter().map(|i| i.start_ms).collect();
    assert!(
        starts.windows(2).all(|w| w[0] <= w[1]),
        "items sorted by start"
    );
    assert_eq!(report.synthesis.cues, 3);
    assert_eq!(
        (report.synthesis.cache_hits, report.synthesis.synthesized),
        (0, 3)
    );
    assert_eq!(report.synthesis.providers_called, ["mock"]);
    assert!(!report.timings_ms.is_empty() && report.timings_ms.contains_key("total"));

    // Subtitles: well-formed files, every caption inside its cue.
    let files = &report.files;
    let formats: Vec<&str> = files.subtitles.iter().map(|f| f.format.as_str()).collect();
    assert_eq!(formats, ["vtt", "srt", "json", "transcript"]);
    let vtt = cue_blocks(&read(&project.join("briefing.vtt")), false);
    let srt = cue_blocks(&read(&project.join("briefing.srt")), true);
    assert!(!vtt.is_empty());
    assert_eq!(vtt.len(), srt.len());
    for ((vs, ve, vl), (ss, se, sl)) in vtt.iter().zip(&srt) {
        assert_eq!((vs, ve), (ss, se));
        assert!(ve > vs);
        assert!(
            vl[0].starts_with("<v ") && sl[0].contains(": "),
            "speaker labels: {vl:?} {sl:?}"
        );
    }
    let words_json = json(&project.join("briefing.words.json"));
    assert_eq!(words_json["version"], "1.0");
    let captions = words_json["captions"].as_array().unwrap();
    assert_eq!(captions.len(), vtt.len());
    for caption in captions {
        let cue = item(&report.items, caption["cue_id"].as_str().unwrap());
        let (start, end) = (
            caption["start_ms"].as_f64().unwrap(),
            caption["end_ms"].as_f64().unwrap(),
        );
        assert!(
            start >= cue.start_ms - 0.05 && start < cue.end_ms,
            "{caption} in {cue:?}"
        );
        // The end may only pass the cue end through the minimum-duration extension (DESIGN.md §6).
        assert!(
            end <= cue.end_ms + 0.05 || end - start <= 800.05,
            "{caption} in {cue:?}"
        );
    }
    let words = words_json["words"].as_array().unwrap();
    assert!(words.len() > 20);
    for w in words {
        let cue = item(&report.items, w["cue_id"].as_str().unwrap());
        let (start, end) = (
            w["start_ms"].as_f64().unwrap(),
            w["end_ms"].as_f64().unwrap(),
        );
        assert!(
            start >= cue.start_ms - 0.05 && end <= cue.end_ms + 0.05 && end >= start,
            "{w}"
        );
    }
    let transcript = json(&project.join("briefing.transcript.json"));
    let transcript = transcript.as_array().unwrap();
    assert_eq!(transcript.len(), words.len());
    assert_eq!(transcript[0]["id"], "w0");

    // Ducking: every spoken word lies inside an active region of the bed.
    assert_eq!(report.ducking.len(), 1);
    let ducking = &report.ducking[0];
    assert_eq!(
        (
            ducking.track_id.as_str(),
            ducking.mode.as_str(),
            ducking.reduction_db
        ),
        ("ambient_bed", "sidechain", 18.0)
    );
    assert_eq!(ducking.triggers, ["voice_primary", "voice_secondary"]);
    for w in words {
        let (start, end) = (
            w["start_ms"].as_f64().unwrap(),
            w["end_ms"].as_f64().unwrap(),
        );
        assert!(
            ducking
                .regions
                .iter()
                .any(|r| r.start_ms <= start && end <= r.end_ms),
            "word {w} not covered by {:?}",
            ducking.regions
        );
    }

    // Stems: one 32-bit float WAV per track, as long as the output.
    assert_eq!(files.stems.len(), 4);
    for stem in &files.stems {
        let path = Path::new(&stem.path);
        assert!(
            path.starts_with(project.join("briefing.stems")),
            "{}",
            stem.path
        );
        let info = soniccraft_dsp::wav::probe(path).unwrap();
        assert_eq!(
            (info.bits_per_sample, info.frames as usize, info.channels),
            (32, frames, 2)
        );
    }

    // Manifest: the report with relative paths and no timings.
    let manifest = json(&project.join("briefing.manifest.json"));
    assert_eq!(
        files.manifest.as_deref(),
        Some(project.join("briefing.manifest.json").to_str().unwrap())
    );
    assert_eq!(manifest["output"]["path"], "briefing.wav");
    assert_eq!(manifest["files"]["subtitles"][0]["path"], "briefing.vtt");
    assert_eq!(
        manifest["files"]["stems"][0]["path"],
        "briefing.stems/ambient_bed.wav"
    );
    assert_eq!(manifest["files"]["preview"], "briefing.preview.html");
    assert!(manifest.get("timings_ms").is_none());
    let parsed: RenderReport =
        serde_json::from_value(manifest.clone()).expect("manifest is a RenderReport");
    assert_eq!(parsed.items, report.items);
    assert!(
        !read(&project.join("briefing.manifest.json")).contains(project.path().to_str().unwrap()),
        "no absolute paths"
    );

    // Preview: one data script with the payload, every `<` escaped.
    let html = read(&project.join("briefing.preview.html"));
    assert!(!html.contains("__SONICCRAFT_DATA__"));
    let open = "<script id=\"soniccraft-data\" type=\"application/json\">";
    let begin = html.find(open).unwrap() + open.len();
    let end = begin + html[begin..].find("</script>").unwrap();
    let data = &html[begin..end];
    assert!(!data.contains('<'));
    let payload: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(payload["soniccraft_preview"], 1);
    assert_eq!(payload["title"], "Tech briefing");
    assert_eq!(payload["audio"], "briefing.wav");
    assert!(payload["report"].get("timings_ms").is_none());
    assert_eq!(payload["report"]["output"]["path"], "briefing.wav");
    let points = (report.output.duration_ms * 50.0 / 1000.0).ceil() as usize;
    assert_eq!(payload["waveform"]["points_per_second"], 50);
    assert_eq!(payload["waveform"]["min"].as_array().unwrap().len(), points);
    assert_eq!(payload["waveform"]["max"].as_array().unwrap().len(), points);
    let envelope = &payload["envelopes"][0];
    assert_eq!(envelope["track_id"], "ambient_bed");
    let gains = envelope["gain_db"].as_array().unwrap();
    assert_eq!(gains.len(), points);
    assert!(gains.iter().any(|g| g.as_f64() == Some(-18.0)));
    assert_eq!(
        payload["captions"].as_array().unwrap().len(),
        captions.len()
    );
    assert_eq!(payload["words"].as_array().unwrap().len(), words.len());
}

/// Ducking depth measured on the exported bed stem, in both detector modes.
#[tokio::test(flavor = "multi_thread")]
async fn ducking_attenuates_the_bed_by_the_configured_depth() {
    for mode in ["sidechain", "cue"] {
        let project = Project::new();
        let text = format!(
            r#"version: "1.0"
output: {{ format: wav, export_stems: true }}
tracks:
  - id: bed
    type: music
    source: assets/audio/bed.wav
    loop: true
    duration_ms: 12000
    normalize: false
    ducking: {{ trigger_tracks: [voice], reduction_db: -18, mode: {mode} }}
  - id: voice
    type: speech
    start_at_ms: 4000
    sequence:
      - text: "Ducking makes the music step aside while someone speaks to you."
        provider: mock
"#
        );
        let source = project.timeline("duck.yaml", &text);
        let report = project
            .engine()
            .render(&source, &RenderOverrides::default())
            .await
            .unwrap();
        let cue = report
            .items
            .iter()
            .find(|i| i.kind == ItemKind::Cue)
            .unwrap();
        assert!(cue.duration_ms > 3000.0, "{cue:?}");
        let stem = decode(&project.join("duck.stems/bed.wav"));
        let f = |ms: f64| (ms * 48.0) as usize;
        // Two windows of one full tremolo period (2 s) each: before the cue, and inside it.
        let open = rms_db(&stem, f(1000.0), f(3000.0));
        let ducked = rms_db(&stem, f(cue.start_ms + 500.0), f(cue.start_ms + 2500.0));
        let depth = open - ducked;
        assert!((depth - 18.0).abs() < 0.5, "{mode}: ducked by {depth} dB");
        let region = &report.ducking[0].regions[0];
        assert!(
            region.start_ms <= cue.start_ms && region.end_ms >= cue.end_ms - 1.0,
            "{mode}: {region:?} vs {cue:?}"
        );
        assert_eq!(report.ducking[0].mode, mode);
    }
}

/// Two independent renders (fresh caches) are byte-identical in every format and side file; a third render
/// from the warm cache produces the same audio.
#[tokio::test(flavor = "multi_thread")]
async fn renders_are_byte_identical() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    for format in ["wav", "flac", "mp3"] {
        let mut runs = Vec::new();
        for run in 0..2 {
            let out_dir = project.join(&format!("out-{format}-{run}"));
            let engine = engine_with_cache(&project.join(&format!("cache-{format}-{run}")));
            let overrides = RenderOverrides {
                output_path: Some(out_dir.join(format!("episode.{format}"))),
                format: Some(format.to_string()),
                export_manifest: Some(true),
                ..RenderOverrides::default()
            };
            let report = engine.render(&source, &overrides).await.unwrap();
            assert_eq!(report.output.format, format);
            runs.push(out_dir);
        }
        for name in [
            format!("episode.{format}"),
            "episode.vtt".to_string(),
            "episode.srt".to_string(),
            "episode.words.json".to_string(),
            "episode.transcript.json".to_string(),
            "episode.manifest.json".to_string(),
        ] {
            let a = std::fs::read(runs[0].join(&name)).unwrap();
            let b = std::fs::read(runs[1].join(&name)).unwrap();
            assert!(a == b, "{name} differs between identical renders");
        }
        // Lossy and lossless files decode to the full length.
        let decoded = decode(&runs[0].join(format!("episode.{format}")));
        let manifest = json(&runs[0].join("episode.manifest.json"));
        let expected =
            (manifest["output"]["duration_ms"].as_f64().unwrap() * 48.0).round() as usize;
        assert_eq!(decoded.frames(), expected, "{format}");
        if format == "mp3" {
            assert_eq!(manifest["output"]["bitrate_bps"], 192_000);
            assert!((lufs(&decoded) + 16.0).abs() < 0.5);
        }
    }

    // Warm cache: same audio, cues now come from the cache.
    let engine = engine_with_cache(&project.join("cache-wav-0"));
    let out_dir = project.join("out-warm");
    let overrides = RenderOverrides {
        output_path: Some(out_dir.join("episode.wav")),
        format: Some("wav".to_string()),
        export_manifest: Some(true),
        ..RenderOverrides::default()
    };
    let warm = engine.render(&source, &overrides).await.unwrap();
    assert_eq!(warm.synthesis.cache_hits, 3);
    assert!(warm.synthesis.providers_called.is_empty());
    assert!(
        warm.items
            .iter()
            .filter(|i| i.kind == ItemKind::Cue)
            .all(|i| i.duration_source == DurationSource::Cache)
    );
    assert!(
        std::fs::read(out_dir.join("episode.wav")).unwrap()
            == std::fs::read(project.join("out-wav-0/episode.wav")).unwrap(),
        "cached render matches the fresh one"
    );
    // The manifest leaves out cache provenance, so it is byte-identical too.
    assert!(
        std::fs::read(out_dir.join("episode.manifest.json")).unwrap()
            == std::fs::read(project.join("out-wav-0/episode.manifest.json")).unwrap(),
        "the manifest of the cached render matches the fresh one"
    );
    let manifest = json(&out_dir.join("episode.manifest.json"));
    assert!(manifest.get("synthesis").is_none());
    let _: RenderReport = serde_json::from_value(manifest).expect("manifest is a RenderReport");
}

/// The plan of a rendered timeline is exact: cached cues have the rendered lengths. Mock cues are exact
/// before the first render too (the plan renders the mock voice in-process, as the render does).
#[tokio::test(flavor = "multi_thread")]
async fn plan_after_render_uses_exact_cached_lengths() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let engine = project.engine();
    let before = engine.plan(&source).await;
    assert!(before.valid && !before.estimated, "{:#?}", before.items);
    assert_eq!((before.synthesis.cues, before.synthesis.cached), (3, 0));
    assert!(before.synthesis.characters_to_synthesize > 200);
    assert!(
        before
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Cue)
            .all(|i| i.duration_source == DurationSource::Exact)
    );

    let report = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap();
    // The cold-cache plan was already exact to the frame.
    assert_eq!(before.duration_ms, report.output.duration_ms);
    for rendered in &report.items {
        let planned = item(&before.items, &rendered.id);
        assert_eq!(
            (planned.start_ms, planned.end_ms),
            (rendered.start_ms, rendered.end_ms),
            "{}",
            rendered.id
        );
    }
    let after = engine.plan(&source).await;
    assert!(after.valid && !after.estimated, "{:?}", after.items);
    assert_eq!(
        (
            after.synthesis.cached,
            after.synthesis.characters_to_synthesize
        ),
        (3, 0)
    );
    assert_eq!(after.synthesis.by_provider["mock"].cached, 3);
    assert_eq!(after.duration_ms, report.output.duration_ms);
    for rendered in &report.items {
        let planned = item(&after.items, &rendered.id);
        assert_eq!(
            (planned.start_ms, planned.end_ms),
            (rendered.start_ms, rendered.end_ms),
            "{}",
            rendered.id
        );
        if rendered.kind == ItemKind::Cue {
            assert_eq!(planned.duration_source, DurationSource::Cache);
        }
    }
    assert_eq!(after.ducking[0].track_id, "ambient_bed");
    assert!(after.diagram.contains("cue_03"));

    // `read_cache: false` (--no-cache) ignores the cache; the mock cues are still exact.
    let no_cache = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        read_cache: false,
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]));
    let fresh = no_cache.plan(&source).await;
    assert_eq!(fresh.synthesis.cached, 0);
    assert!(!fresh.estimated);
    assert!(
        fresh
            .items
            .iter()
            .filter(|i| i.kind == ItemKind::Cue)
            .all(|i| i.duration_source == DurationSource::Exact)
    );
}

/// A cancelled render returns `cancelled: true` and leaves no output behind, wherever it stops.
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_renders_leave_nothing_behind() {
    for stage in ["before", "synthesize", "mix", "master", "export"] {
        let project = Project::new();
        let source = project.timeline("briefing.yaml", README);
        let flag = Arc::new(AtomicBool::new(stage == "before"));
        let trigger = Arc::clone(&flag);
        let target = stage.to_string();
        let engine = Engine::new(EngineOptions {
            cache_dir: Some(project.cache_dir()),
            progress: Some(Arc::new(move |event: &ProgressEvent| {
                if let ProgressEvent::Stage { stage, .. } = event
                    && *stage == target
                {
                    trigger.store(true, Ordering::Relaxed);
                }
            })),
            ..EngineOptions::default()
        })
        .with_env(hermetic_env(&[]));
        let overrides = RenderOverrides {
            export_stems: Some(true),
            preview: true,
            cancel: Some(flag),
            ..RenderOverrides::default()
        };
        let failure = engine.render(&source, &overrides).await.unwrap_err();
        assert!(failure.cancelled, "{stage}: {failure:?}");
        assert_eq!(failure.message, "render cancelled");
        assert!(failure.diagnostics.is_empty());
        let mut left: Vec<String> = std::fs::read_dir(project.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("briefing.") && n != "briefing.yaml")
            .collect();
        left.sort();
        assert!(left.is_empty(), "{stage}: left behind {left:?}");
    }
}

/// Inline timelines render next to their base directory as `soniccraft-output.<ext>`; a mono render works.
#[tokio::test(flavor = "multi_thread")]
async fn inline_mono_render() {
    let project = Project::new();
    let text = r#"version: "1.0"
output: { format: flac, channels: 1, bit_depth: 16, sample_rate: 44100 }
tracks:
  - id: voice
    type: speech
    sequence:
      - text: "Hello mono world."
        provider: mock
  - id: whoosh
    type: sfx
    source: assets/sfx/whoosh.wav
    start_relative: { after_cue: voice, offset_ms: -200 }
"#;
    let report = project
        .engine()
        .render(&inline(text, project.path()), &RenderOverrides::default())
        .await
        .unwrap();
    let out = project.join("soniccraft-output.flac");
    assert_eq!(Path::new(&report.output.path), out);
    let audio = decode(&out);
    assert_eq!((audio.num_channels(), audio.sample_rate), (1, 44_100));
    assert_eq!(report.output.bit_depth, Some(16));
    assert!((lufs(&audio) + 16.0).abs() < 0.3);
}

/// Speech overlaps under `speech_overlap: error` fail the render with the layout diagnostic.
#[tokio::test(flavor = "multi_thread")]
async fn overlap_policy_error_fails_the_render() {
    let project = Project::new();
    let text = r#"version: "1.0"
timing: { speech_overlap: error }
tracks:
  - id: a
    type: speech
    sequence:
      - { id: one, text: "We both talk at the same time.", provider: mock }
  - id: b
    type: speech
    sequence:
      - { id: two, text: "Yes we do, right now.", provider: mock, start_at_ms: 300 }
"#;
    let source = project.timeline("overlap.yaml", text);
    let failure = project
        .engine()
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    assert!(!failure.cancelled);
    let overlap = failure
        .diagnostics
        .iter()
        .find(|d| d.code == "speech_overlap")
        .unwrap_or_else(|| panic!("{failure:?}"));
    assert!(overlap.is_error() && overlap.hint.is_some() && overlap.line.is_some());
    assert!(!project.join("overlap.wav").exists());
}

/// Progress: every stage in pipeline order, one `synthesized` event per cue.
#[tokio::test(flavor = "multi_thread")]
async fn progress_events_follow_the_pipeline() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        progress: Some(Arc::new(move |event: &ProgressEvent| {
            sink.lock().unwrap().push(event.clone());
        })),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]));
    engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap();
    let events = events.lock().unwrap();
    let stages: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ProgressEvent::Stage { stage, .. } => Some(stage.as_str()),
            ProgressEvent::Synthesized { .. } => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            "parse",
            "decode",
            "synthesize",
            "process",
            "layout",
            "mix",
            "master",
            "encode",
            "export"
        ]
    );
    let mut synthesized: Vec<(String, usize, usize)> = events
        .iter()
        .filter_map(|e| match e {
            ProgressEvent::Synthesized {
                cue_id,
                done,
                total,
                cached,
            } => {
                assert!(!cached);
                Some((cue_id.clone(), *done, *total))
            }
            ProgressEvent::Stage { .. } => None,
        })
        .collect();
    synthesized.sort();
    let ids: Vec<&str> = synthesized.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(ids, ["cue_01", "cue_02", "cue_03"]);
    assert!(
        synthesized
            .iter()
            .all(|s| s.2 == 3 && (1..=3).contains(&s.1))
    );
}

/// Outputs never overwrite the timeline or an asset, and a bad format override is a precise error.
#[tokio::test(flavor = "multi_thread")]
async fn unsafe_outputs_and_bad_overrides_are_refused() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let engine = project.engine();
    let attempt = |output: &str, format: Option<&str>| RenderOverrides {
        output_path: Some(project.join(output)),
        format: format.map(str::to_string),
        ..RenderOverrides::default()
    };

    let failure = engine
        .render(&source, &attempt("briefing.yaml", Some("wav")))
        .await
        .unwrap_err();
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|d| d.message.contains("would overwrite the timeline file")),
        "{failure:#?}"
    );
    assert!(
        read(&project.join("briefing.yaml")) == README,
        "the timeline is untouched"
    );

    let asset_bytes = std::fs::read(project.join("assets/sfx/whoosh.wav")).unwrap();
    let failure = engine
        .render(&source, &attempt("assets/sfx/whoosh.wav", None))
        .await
        .unwrap_err();
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|d| d.message.contains("would overwrite the audio asset")),
        "{failure:#?}"
    );
    assert_eq!(
        std::fs::read(project.join("assets/sfx/whoosh.wav")).unwrap(),
        asset_bytes
    );

    let failure = engine
        .render(&source, &attempt("assets", Some("wav")))
        .await
        .unwrap_err();
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|d| d.message.contains("is a directory")),
        "{failure:#?}"
    );

    let failure = engine
        .render(&source, &attempt("out.mp3", Some("mp4a")))
        .await
        .unwrap_err();
    let d = &failure.diagnostics[0];
    assert_eq!(d.code, "invalid_value");
    assert!(
        d.message.contains("mp4a") && d.hint.as_deref().unwrap().contains("`wav`"),
        "{d:?}"
    );

    // Missing directories are created. The document's explicit `format: wav` wins over the extension of
    // the override (DESIGN.md §2.3), with a warning.
    let report = engine
        .render(&source, &attempt("nested/dir/episode.flac", None))
        .await
        .unwrap();
    assert_eq!(
        Path::new(&report.output.path),
        project.join("nested/dir/episode.flac")
    );
    assert_eq!(report.output.format, "wav");
    assert!(project.join("nested/dir/episode.vtt").exists());
    let mismatch = report
        .diagnostics
        .iter()
        .find(|d| d.code == "format_extension_mismatch")
        .unwrap_or_else(|| panic!("{:#?}", report.diagnostics));
    assert!(
        mismatch.hint.as_deref().unwrap().contains("episode.wav"),
        "{mismatch:?}"
    );
}
