//! Shared fixtures for the engine integration tests: a temp project with procedurally generated assets,
//! timeline helpers and measurement helpers.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use soniccraft_core::SourceFormat;
use soniccraft_dsp::AudioBuffer;
use soniccraft_dsp::wav::{self, WavEncoding};
use soniccraft_engine::{Engine, EngineOptions, EnvSource, TimelineSource};

/// The README timeline (DESIGN.md §3.4 worked example) with the deterministic mock voice and generated
/// assets: an open-ended ducked bed, a whoosh, cue_02 anchored after cue_01, cue_03 pushed after cue_02.
pub const README: &str = r#"version: "1.0"
title: "Tech briefing"
output:
  format: wav
  sample_rate: 48000
  target_lufs: -16.0
  export_subtitles: [vtt, srt, json, transcript]
  export_manifest: true
tracks:
  - id: ambient_bed
    type: music
    source: assets/audio/bed.wav
    loop: true
    volume: 0.65
    fade_in_ms: 1200
    fade_out_ms: 2500
    ducking:
      trigger_tracks: [voice_primary, voice_secondary]
      reduction_db: -18.0
      attack_ms: 80
      release_ms: 350
  - id: voice_primary
    type: speech
    pan: -0.15
    sequence:
      - id: cue_01
        speaker: narrator
        provider: mock
        voice_id: narrator
        text: "Autonomous software development requires deterministic compilation targets."
      - pause_duration: 350ms
      - id: cue_03
        speaker: narrator
        provider: mock
        voice_id: narrator
        text: "By separating high-level planning from digital signal processing, systems eliminate runtime hallucinations."
  - id: voice_secondary
    type: speech
    pan: 0.15
    sequence:
      - id: cue_02
        start_relative:
          after_cue: cue_01
          offset_ms: 150
        speaker: guest
        provider: mock
        voice_id: guest
        text: "Exactly. SonicCraft treats audio composition the same way Hyperframes treated web-based video."
  - id: sfx_impact
    type: sfx
    source: assets/sfx/whoosh.wav
    volume: 0.8
    start_at_ms: 0
"#;

/// A temporary project directory with generated assets.
pub struct Project {
    pub dir: tempfile::TempDir,
}

impl Project {
    /// A project with `assets/audio/bed.wav` (4 s seamless stereo chord loop) and `assets/sfx/whoosh.wav`
    /// (1.2 s stereo noise swell), both 48 kHz 16-bit.
    pub fn new() -> Project {
        let dir = tempfile::tempdir().expect("temp dir");
        let project = Project { dir };
        project.write_audio("assets/audio/bed.wav", &bed(48_000, 4.0));
        project.write_audio("assets/sfx/whoosh.wav", &whoosh(48_000, 1.2));
        project
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Write a 16-bit WAV asset.
    pub fn write_audio(&self, rel: &str, buffer: &AudioBuffer) {
        let path = self.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        wav::write(&path, buffer, WavEncoding::Int16 { dither: false }).unwrap();
    }

    /// Write a timeline file and return its source.
    pub fn timeline(&self, name: &str, text: &str) -> TimelineSource {
        let path = self.join(name);
        std::fs::write(&path, text).unwrap();
        TimelineSource::from_path(&path).unwrap()
    }

    /// The cache directory the tests use.
    pub fn cache_dir(&self) -> PathBuf {
        self.join("cache")
    }

    /// An engine with this project's cache and a hermetic environment (only `PATH` and `HOME`).
    pub fn engine(&self) -> Engine {
        engine_with_cache(&self.cache_dir())
    }
}

/// An engine with `cache` and only `PATH`/`HOME` from the process environment.
pub fn engine_with_cache(cache: &Path) -> Engine {
    Engine::new(EngineOptions {
        cache_dir: Some(cache.to_path_buf()),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]))
}

/// `PATH` and `HOME` from the process plus `extra`.
pub fn hermetic_env(extra: &[(&str, &str)]) -> EnvSource {
    let mut vars: Vec<(String, String)> = ["PATH", "HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    vars.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    EnvSource::fixed(vars)
}

/// An inline source (YAML) resolved against `base`.
pub fn inline(text: &str, base: &Path) -> TimelineSource {
    TimelineSource::from_text(text, base, SourceFormat::Yaml)
}

/// A seamless stereo loop: an A-minor chord whose partials complete whole cycles over the loop.
pub fn bed(rate: u32, secs: f64) -> AudioBuffer {
    let n = (secs * f64::from(rate)).round() as usize;
    // 0.25 Hz resolution keeps every partial periodic over a 4 s loop.
    let partials = [(220.0, 0.30), (261.5, 0.22), (329.75, 0.20), (440.0, 0.10)];
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let tremolo = 0.85 + 0.15 * (2.0 * std::f64::consts::PI * 0.5 * t).sin();
        let mut l = 0.0;
        let mut r = 0.0;
        for (k, (f, a)) in partials.iter().enumerate() {
            let phase = 2.0 * std::f64::consts::PI * f * t;
            l += a * phase.sin();
            r += a * (phase + 0.3 * k as f64).sin();
        }
        left.push((0.25 * tremolo * l) as f32);
        right.push((0.25 * tremolo * r) as f32);
    }
    AudioBuffer::new(rate, vec![left, right])
}

/// A noise swell (deterministic xorshift noise, low-passed, raised-sine envelope).
pub fn whoosh(rate: u32, secs: f64) -> AudioBuffer {
    let n = (secs * f64::from(rate)).round() as usize;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    };
    let (mut lp_l, mut lp_r) = (0.0f64, 0.0f64);
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let x = i as f64 / n as f64;
        let envelope = (std::f64::consts::PI * x).sin().powi(2);
        let cutoff = 0.02 + 0.3 * envelope;
        lp_l += cutoff * (next() - lp_l);
        lp_r += cutoff * (next() - lp_r);
        left.push((0.8 * envelope * lp_l) as f32);
        right.push((0.8 * envelope * lp_r) as f32);
    }
    AudioBuffer::new(rate, vec![left, right])
}

/// Decode an output file.
pub fn decode(path: &Path) -> AudioBuffer {
    soniccraft_dsp::decode::decode_file(path)
        .unwrap_or_else(|e| panic!("decode {}: {e}", path.display()))
}

/// Integrated loudness of a buffer.
pub fn lufs(buffer: &AudioBuffer) -> f64 {
    soniccraft_dsp::loudness::integrated_lufs(buffer)
        .unwrap()
        .expect("measurable loudness")
}

/// True peak of a buffer in dBTP.
pub fn true_peak_dbtp(buffer: &AudioBuffer) -> f64 {
    soniccraft_dsp::loudness::true_peak_dbtp(buffer)
        .unwrap()
        .expect("non-silent audio")
}

/// RMS in dBFS of frames `[from, to)` of channel 0.
pub fn rms_db(buffer: &AudioBuffer, from: usize, to: usize) -> f64 {
    let x = &buffer.channels[0][from.min(buffer.frames())..to.min(buffer.frames())];
    let ms: f64 =
        x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / x.len().max(1) as f64;
    10.0 * ms.max(1e-30).log10()
}

/// Read a file to a string.
pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Read and parse a JSON file.
pub fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&read(path)).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}
