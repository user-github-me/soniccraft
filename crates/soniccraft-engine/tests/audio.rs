//! Audio correctness and what reports say about it: delivered (lossy) loudness, missed loudness targets,
//! repaired and downmixed sources, unsupported sample rates, cache write failures and preview determinism.

mod common;

use std::path::Path;

use common::*;
use soniccraft_core::Diagnostic;
use soniccraft_dsp::ffmpeg::Ffmpeg;
use soniccraft_engine::{Engine, EngineOptions, RenderOverrides, Severity, ValidateOptions};

fn find<'a>(report: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    report
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("no `{code}` in {report:#?}"))
}

/// A WAV file from raw little-endian sample bytes (format tag 1 = PCM, 3 = IEEE float; no channel mask).
fn wav_bytes(rate: u32, channels: u16, tag: u16, bits: u16, data: &[u8]) -> Vec<u8> {
    let block = channels * bits / 8;
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn sine(i: usize, rate: u32, freq: f64) -> f64 {
    (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin()
}

/// For lossy formats the report describes the delivered file: its loudness and true peak are what the
/// decoded file measures, and the master that went into the encoder is in `pre_encode`.
#[tokio::test(flavor = "multi_thread")]
async fn lossy_reports_describe_the_delivered_file() {
    let project = Project::new();
    // A hot master: codecs overshoot the true-peak ceiling the most here.
    let source = project.timeline(
        "hot.yaml",
        &README.replace("target_lufs: -16.0", "target_lufs: -9.0"),
    );
    let engine = project.engine();
    let ffmpeg = soniccraft_dsp::ffmpeg::find_ffmpeg().is_some();
    let mut checked = 0;
    for format in ["mp3", "m4a", "opus", "ogg"] {
        let native = format == "mp3" && cfg!(feature = "mp3");
        if !native && !ffmpeg {
            eprintln!("skipping {format}: ffmpeg is not installed");
            continue;
        }
        let path = project.join(&format!("out/hot.{format}"));
        let report = engine
            .render(
                &source,
                &RenderOverrides {
                    output_path: Some(path.clone()),
                    format: Some(format.to_string()),
                    export_subtitles: Some(false),
                    ..RenderOverrides::default()
                },
            )
            .await
            .unwrap_or_else(|f| panic!("{format}: {f:#?}"));
        let measured = soniccraft_dsp::loudness::measure_file(&path, &Ffmpeg::Auto).unwrap();
        let loudness = &report.loudness;
        assert_eq!(
            loudness.integrated_lufs, measured.integrated_lufs,
            "{format}"
        );
        assert_eq!(loudness.true_peak_dbtp, measured.true_peak_dbtp, "{format}");
        assert_eq!(
            loudness.sample_peak_dbfs, measured.sample_peak_dbfs,
            "{format}"
        );
        let pre = loudness
            .pre_encode
            .as_ref()
            .unwrap_or_else(|| panic!("{format}"));
        assert!(pre.compensation_passes <= 3);
        if pre.compensation_passes > 0 {
            assert!(
                pre.ceiling_dbtp < loudness.true_peak_target_dbtp,
                "{format}: {pre:?}"
            );
        } else {
            assert_eq!(pre.ceiling_dbtp, loudness.true_peak_target_dbtp);
        }
        // A delivered file that misses the target the master had reached says so.
        let missed = (loudness.target_lufs - measured.integrated_lufs.unwrap()).abs() > 0.2;
        assert_eq!(
            missed,
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "loudness_target_missed"),
            "{format}: {:#?}",
            report.diagnostics
        );
        // Messages and hints are single-spaced prose (a broken `\` continuation once left runs of the
        // source indentation inside the delivered-file `loudness_target_missed` text).
        for d in &report.diagnostics {
            for text in std::iter::once(d.message.as_str()).chain(d.hint.as_deref()) {
                assert!(!text.contains("  "), "{format}: `{}`: {text:?}", d.code);
            }
        }
        let over = measured.true_peak_dbtp.unwrap() > loudness.true_peak_target_dbtp;
        assert_eq!(
            over,
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "true_peak_exceeded"),
            "{format}: {measured:?} {:#?}",
            report.diagnostics
        );
        eprintln!(
            "{format}: delivered {:.2} LUFS / {:.2} dBTP, master {:.2} LUFS / {:.2} dBTP, {} passes",
            measured.integrated_lufs.unwrap(),
            measured.true_peak_dbtp.unwrap(),
            pre.integrated_lufs.unwrap(),
            pre.true_peak_dbtp.unwrap(),
            pre.compensation_passes
        );
        checked += 1;
    }
    if checked == 0 {
        return;
    }
    // Lossless files carry the master itself.
    let wav = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap();
    assert!(wav.loudness.pre_encode.is_none());
}

/// Lossy codecs shift integrated loudness by a few tenths of a LU (the reviewers measured 0.27 LU at 192 kbps
/// MP3): the delivered file is brought back within the 0.2 LU tolerance of the target, or the render says
/// why not (a codec whose peaks the ceiling could not contain wins over loudness).
#[tokio::test(flavor = "multi_thread")]
async fn lossy_files_land_on_the_loudness_target() {
    let project = Project::new();
    let source = project.timeline(
        "podcast.yaml",
        &README.replace("target_lufs: -16.0", "target_lufs: -16.0\n  bitrate: 128k"),
    );
    let engine = project.engine();
    let ffmpeg = soniccraft_dsp::ffmpeg::find_ffmpeg().is_some();
    for format in ["mp3", "ogg", "m4a", "opus"] {
        let native = format == "mp3" && cfg!(feature = "mp3");
        if !native && !ffmpeg {
            eprintln!("skipping {format}: ffmpeg is not installed");
            continue;
        }
        let path = project.join(&format!("out/podcast.{format}"));
        let report = engine
            .render(
                &source,
                &RenderOverrides {
                    output_path: Some(path.clone()),
                    format: Some(format.to_string()),
                    export_subtitles: Some(false),
                    ..RenderOverrides::default()
                },
            )
            .await
            .unwrap_or_else(|f| panic!("{format}: {f:#?}"));
        let measured = soniccraft_dsp::loudness::measure_file(&path, &Ffmpeg::Auto).unwrap();
        let lufs = measured.integrated_lufs.unwrap();
        let peak = measured.true_peak_dbtp.unwrap();
        let has = |code: &str| report.diagnostics.iter().any(|d| d.code == code);
        eprintln!(
            "{format}: delivered {lufs:.2} LUFS / {peak:.2} dBTP after {} passes",
            report
                .loudness
                .pre_encode
                .as_ref()
                .unwrap()
                .compensation_passes
        );
        let on_target = (lufs - report.loudness.target_lufs).abs() <= 0.2;
        assert_eq!(!on_target, has("loudness_target_missed"), "{format}");
        assert_eq!(
            peak > report.loudness.true_peak_target_dbtp,
            has("true_peak_exceeded"),
            "{format}"
        );
        if format == "mp3" {
            // LAME keeps its peaks: the drift correction alone brings it onto the target.
            assert!(on_target, "mp3: delivered {lufs:.2} LUFS");
            assert!(!has("true_peak_exceeded"), "{:#?}", report.diagnostics);
        }
    }
}

/// The reviewers' ticking clock: the mastering loop cannot land within 0.2 LU, so the render says how far
/// it got and how to accept it.
#[tokio::test(flavor = "multi_thread")]
async fn a_missed_loudness_target_is_reported() {
    let project = Project::new();
    // 8 s stereo 48 kHz: a 2.5 kHz tick with a 40-sample decay every 500 ms, 16-bit (verify/review-dsp).
    let rate = 48_000u32;
    let mut data = Vec::new();
    for i in 0..(rate as usize * 8) {
        let k = i % (rate as usize / 2);
        let v = if k < 400 {
            sine(k, rate, 2500.0) * (-(k as f64) / 40.0).exp()
        } else {
            0.0
        };
        let s = (v * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(project.join("tick.wav"), wav_bytes(rate, 2, 1, 16, &data)).unwrap();
    let text = r#"version: "1.0"
output: { format: wav, bit_depth: 16, loudness_preset: podcast }
tracks:
  - id: clock
    type: sfx
    source: tick.wav
    loop: true
    volume: 1.0
    fade_out_ms: 500
  - id: voice
    type: speech
    sequence:
      - { text: "The clock is ticking. We have very little time left to finish this.", provider: mock }
      - pause: 4s
      - { text: "Tick tock.", provider: mock }
"#;
    let report = project
        .engine()
        .render(
            &project.timeline("clock.yaml", text),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    let achieved = report.loudness.integrated_lufs.unwrap();
    assert!((achieved + 16.0).abs() > 0.2, "{achieved}");
    let d = find(&report.diagnostics, "loudness_target_missed");
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Warning, Some("output.target_lufs"))
    );
    assert!(
        d.message
            .starts_with(&format!("mastering stopped at {achieved:.1} LUFS")),
        "{}",
        d.message
    );
    assert_eq!(
        d.hint.as_deref(),
        Some(
            format!(
                "Set `output.target_lufs` to {achieved:.1} to accept this level, or even out the mix (raise \
                 quiet passages, lower loud transients)"
            )
            .as_str()
        )
    );
}

/// Damaged and wide sources are repaired and said so; a sample rate no decoder should trust is refused at
/// validate and at render, with a resampling hint.
#[tokio::test(flavor = "multi_thread")]
async fn sources_are_repaired_downmixed_or_refused() {
    let project = Project::new();
    // 3 s float32 stereo sine with one NaN and one +∞ frame, at 44.1 kHz (resampled) and 48 kHz.
    for (name, rate) in [("nan44.wav", 44_100u32), ("nan48.wav", 48_000)] {
        let mut data = Vec::new();
        for i in 0..(rate as usize * 3) {
            let v = match i {
                i if i == rate as usize => f32::NAN,
                i if i == rate as usize * 2 => f32::INFINITY,
                _ => (0.3 * sine(i, rate, 330.0)) as f32,
            };
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(project.join(name), wav_bytes(rate, 2, 3, 32, &data)).unwrap();
    }
    // 2 s of 5.1 with the tone only in the centre channel.
    let mut data = Vec::new();
    for i in 0..96_000usize {
        let v = (0.5 * sine(i, 48_000, 440.0) * 32767.0).round() as i16;
        for ch in 0..6 {
            let s = if ch == 2 { v } else { 0 };
            data.extend_from_slice(&s.to_le_bytes());
        }
    }
    std::fs::write(
        project.join("centre51.wav"),
        wav_bytes(48_000, 6, 1, 16, &data),
    )
    .unwrap();
    let engine = project.engine();
    for (asset, code) in [
        ("nan44.wav", "audio_repaired"),
        ("nan48.wav", "audio_repaired"),
        ("centre51.wav", "channels_downmixed"),
    ] {
        let text = format!(
            "version: \"1.0\"\noutput: {{ format: wav }}\ntracks:\n  - {{ id: m, type: music, source: {asset} }}\n"
        );
        let report = engine
            .render(
                &project.timeline("src.yaml", &text),
                &RenderOverrides::default(),
            )
            .await
            .unwrap_or_else(|f| panic!("{asset}: {f:#?}"));
        let d = find(&report.diagnostics, code);
        assert_eq!(d.path.as_deref(), Some("tracks[0].source"), "{asset}");
        assert!(d.message.contains(asset), "{}", d.message);
        let lufs = report.loudness.integrated_lufs.unwrap();
        assert!((lufs + 16.0).abs() < 0.3, "{asset}: {lufs} LUFS");
        if code == "audio_repaired" {
            assert_eq!(d.severity, Severity::Warning);
            // Two bad frames of a stereo file.
            assert!(d.message.contains("4 non-finite samples"), "{}", d.message);
            assert!(report.diagnostics.iter().all(|d| d.code != "silent_output"));
        } else {
            assert_eq!(d.severity, Severity::Info);
            assert!(
                d.message.contains("6 channels") && d.message.contains("LFE dropped"),
                "{}",
                d.message
            );
        }
    }
    // A header that claims 4 MHz.
    let data: Vec<u8> = (0..4000i16).flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(
        project.join("odd.wav"),
        wav_bytes(4_000_037, 1, 1, 16, &data),
    )
    .unwrap();
    let text = "version: \"1.0\"\ntracks:\n  - { id: m, type: music, source: odd.wav }\n";
    let source = project.timeline("odd.yaml", text);
    let report = engine.validate(&source, &ValidateOptions::default()).await;
    let d = find(&report.diagnostics, "asset_unreadable");
    assert!(
        d.message.contains("unsupported sample rate 4000037 Hz"),
        "{}",
        d.message
    );
    assert!(
        !d.message.contains(project.path().to_str().unwrap()),
        "no absolute path: {}",
        d.message
    );
    assert!(d.hint.as_deref().unwrap().contains("-ar 48000"), "{d:?}");
}

/// A cache that cannot be written is one warning per render (not a provider note), and it stays out of the
/// manifest.
#[tokio::test(flavor = "multi_thread")]
async fn cache_write_failures_are_one_warning() {
    let project = Project::new();
    let blocked = project.join("cache-is-a-file");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(blocked),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]));
    let report = engine
        .render(
            &project.timeline("briefing.yaml", README),
            &RenderOverrides::default(),
        )
        .await
        .unwrap();
    let warnings: Vec<&Diagnostic> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "cache_write_failed")
        .collect();
    assert_eq!(warnings.len(), 1, "{:#?}", report.diagnostics);
    let d = warnings[0];
    assert_eq!(d.severity, Severity::Warning);
    assert!(d.message.contains("3 cues"), "{}", d.message);
    assert!(d.hint.as_deref().unwrap().contains("--cache-dir"), "{d:?}");
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| !(d.code == "setting_ignored" && d.message.contains("cache"))),
        "{:#?}",
        report.diagnostics
    );
    let manifest = read(&project.join("briefing.manifest.json"));
    assert!(!manifest.contains("cache_write_failed"));
}

/// Fresh and warm renders write the same preview page (the payload is the manifest's report).
#[tokio::test(flavor = "multi_thread")]
async fn previews_do_not_depend_on_the_cache() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let engine = project.engine();
    let mut pages = Vec::new();
    for run in ["fresh", "warm"] {
        let report = engine
            .render(
                &source,
                &RenderOverrides {
                    output_path: Some(project.join(&format!("{run}/demo.wav"))),
                    preview: true,
                    ..RenderOverrides::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(report.synthesis.cache_hits == 3, run == "warm");
        pages.push(std::fs::read(project.join(&format!("{run}/demo.preview.html"))).unwrap());
        for name in ["demo.wav", "demo.manifest.json"] {
            assert!(Path::new(&project.join(&format!("{run}/{name}"))).exists());
        }
    }
    assert!(pages[0] == pages[1], "preview.html depends on the cache");
    let html = String::from_utf8(pages.remove(0)).unwrap();
    let open = "<script id=\"soniccraft-data\" type=\"application/json\">";
    let begin = html.find(open).unwrap() + open.len();
    let end = begin + html[begin..].find("</script>").unwrap();
    let payload: serde_json::Value = serde_json::from_str(&html[begin..end]).unwrap();
    let report = &payload["report"];
    assert!(report.get("synthesis").is_none(), "no synthesis counters");
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["duration_source"] != "cache")
    );
    let manifest = json(&project.join("warm/demo.manifest.json"));
    assert_eq!(
        report, &manifest,
        "the preview embeds the manifest's report"
    );
}
