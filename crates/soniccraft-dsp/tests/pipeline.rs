//! End-to-end composition of the building blocks the way the engine uses them (DESIGN.md §4):
//! decode → resample → trim → normalize → place → pan → duck → mix → de-click → master → encode.

use std::path::{Path, PathBuf};

use soniccraft_dsp::duck::{DuckEnvelope, DuckParams};
use soniccraft_dsp::encode::{self, EncodeFormat, EncodeOptions};
use soniccraft_dsp::master::{self, MasterParams};
use soniccraft_dsp::render::{self, ClipSpec, FadeCurve};
use soniccraft_dsp::{AudioBuffer, analysis, decode, loudness, resample, waveform};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

const SR: u32 = 48_000;

/// §4.1: one speech cue, ready to place.
fn prepare_cue(path: &Path, target_lufs: f64) -> (AudioBuffer, usize) {
    let decoded = decode::decode_file(path).unwrap().to_mono();
    // Simulate a TTS engine's leading/trailing silence (0.25 s each side).
    let pad = vec![0.0f32; decoded.sample_rate as usize / 4];
    let decoded = AudioBuffer::mono(
        decoded.sample_rate,
        [pad.clone(), decoded.channels[0].clone(), pad].concat(),
    );
    let resampled = resample::resample(&decoded, SR).unwrap();
    assert_eq!(
        resampled.frames(),
        resample::output_len(decoded.frames(), decoded.sample_rate, SR)
    );
    let padding = (0.030 * f64::from(SR)).round() as usize;
    let bounds = analysis::trim_bounds(&resampled, -50.0, padding).expect("speech is not silent");
    let mut cue = resampled.slice(bounds.start, bounds.end);
    let gain =
        loudness::normalization_gain_db(loudness::integrated_lufs(&cue).unwrap(), target_lufs)
            .unwrap();
    cue.apply_gain(analysis::db_to_linear(gain) as f32);
    (cue, bounds.start)
}

/// A seamless-ish 2 s stereo "music" loop at 44.1 kHz.
fn music_loop() -> AudioBuffer {
    let rate = 44_100u32;
    let n = 2 * rate as usize;
    let chord = |i: usize, detune: f64| {
        let t = i as f64 / f64::from(rate);
        [220.0, 277.18, 329.63]
            .iter()
            .map(|f| (2.0 * std::f64::consts::PI * f * detune * t).sin())
            .sum::<f64>()
            / 3.0
    };
    let l = (0..n).map(|i| (0.3 * chord(i, 1.0)) as f32).collect();
    let r = (0..n).map(|i| (0.3 * chord(i, 1.003)) as f32).collect();
    AudioBuffer::new(rate, vec![l, r])
}

#[test]
fn render_a_ducked_podcast_segment() {
    let (cue, trimmed_from) = prepare_cue(&fixture("say_fllr.wav"), -20.0);
    assert!(
        trimmed_from >= ((0.25 - 0.031) * f64::from(SR)) as usize,
        "leading silence was trimmed: {trimmed_from}"
    );
    let cue_len = cue.frames();
    let cue1_start = SR as usize; // 1.0 s
    let cue2_start = cue1_start + cue_len + (0.35 * f64::from(SR)) as usize;
    let content_end = cue2_start + cue_len;
    let total = content_end + SR as usize; // 1 s tail = bed fade-out

    // Speech track: mono cues → stereo stem (pan 0); the detector is the pre-fader mono content.
    let mut speech_stem = AudioBuffer::silent(SR, 2, total);
    let mut detector = AudioBuffer::silent(SR, 1, total);
    for start in [cue1_start, cue2_start] {
        render::mix_into(&mut speech_stem, &cue, start as i64, 1.0, 0.0).unwrap();
        render::mix_into(&mut detector, &cue, start as i64, 1.0, 0.0).unwrap();
    }

    // Music track: an open-ended looped bed, normalized once per source (§4.2), fading out at the end.
    let source = resample::resample(&music_loop(), SR).unwrap();
    let source_gain =
        loudness::normalization_gain_db(loudness::integrated_lufs(&source).unwrap(), -20.0)
            .unwrap();
    let bed = ClipSpec {
        looped: true,
        loop_crossfade: (0.05 * f64::from(SR)) as usize,
        fade_in: (0.5 * f64::from(SR)) as usize,
        fade_out: SR as usize,
        fade_curve: FadeCurve::EqualPower,
        gain: analysis::db_to_linear(source_gain) as f32,
        ..ClipSpec::new(0, total)
    };
    let mut music_stem = AudioBuffer::silent(SR, 2, total);
    render::mix_clip(&mut music_stem, &source, &bed, 0.0).unwrap();
    music_stem.apply_gain(analysis::db_to_linear(-8.0) as f32); // track fader

    // Ducking (§4.5) with the DSL defaults.
    let params = DuckParams::default();
    let envelope = DuckEnvelope::sidechain(&detector, total, &params);
    let regions = envelope.regions();
    // The 350 ms pause (plus trim padding) outlasts hold (250 ms) + lookahead (80 ms): two regions.
    assert_eq!(regions.len(), 2, "{regions:?}");
    let lookahead = (0.080 * f64::from(SR)) as usize;
    let near = (0.060 * f64::from(SR)) as usize; // trim padding + detector onset
    for (region, cue_start) in regions.iter().zip([cue1_start, cue2_start]) {
        let onset = region.start + lookahead;
        assert!(
            onset >= cue_start && onset <= cue_start + near,
            "{region:?} vs cue at {cue_start}"
        );
    }
    let hold = (0.250 * f64::from(SR)) as usize;
    assert!(
        regions[1].end >= content_end - near + hold && regions[1].end <= content_end + hold,
        "{regions:?}"
    );
    let unducked = music_stem.clone();
    envelope.apply(&mut music_stem).unwrap();
    let mid = cue1_start + cue_len / 2;
    let ratio = music_stem.channels[0][mid] / unducked.channels[0][mid];
    assert!(
        (20.0 * f64::from(ratio).log10() + 15.0).abs() < 1e-3,
        "fully ducked mid-cue: {ratio}"
    );
    assert_eq!(
        music_stem.channels[0][SR as usize / 2],
        unducked.channels[0][SR as usize / 2],
        "untouched before"
    );
    let gains = envelope.sample_gain_db(50);
    assert_eq!(
        gains.len(),
        (total as u64 * 50).div_ceil(u64::from(SR)) as usize
    );
    assert!(gains.iter().any(|&g| g == -15.0));

    // Master bus (§4.6).
    let mut mix = AudioBuffer::silent(SR, 2, total);
    for stem in [&speech_stem, &music_stem] {
        render::mix_into(&mut mix, stem, 0, 1.0, 0.0).unwrap();
    }
    render::declick_tail(&mut mix);
    let result = master::master(
        &mut mix,
        &MasterParams {
            target_lufs: -16.0,
            true_peak_db: -1.0,
            normalize: true,
        },
    )
    .unwrap();
    assert!(result.target_reached, "{result:?}");
    assert!(result.after.true_peak_dbtp.unwrap() <= -1.0);

    // Preview waveform and encodes.
    let wf = waveform::waveform(&mix, 50);
    assert_eq!(wf.min.len(), gains.len());
    let dir = tempfile::tempdir().unwrap();
    let mut formats = vec![
        EncodeFormat::Flac { bit_depth: 16 },
        EncodeFormat::Wav { bit_depth: 24 },
    ];
    if cfg!(feature = "mp3") {
        formats.push(EncodeFormat::Mp3 {
            bitrate_bps: 192_000,
        });
    }
    for format in formats {
        let path = dir.path().join(format!("segment.{}", format.name()));
        let summary = encode::encode_file(&mix, &path, &EncodeOptions::new(format)).unwrap();
        assert!(summary.bytes > 0);
        let back = decode::decode_file(&path).unwrap();
        assert_eq!(back.frames(), total, "{format:?}: exact length");
        let i = loudness::integrated_lufs(&back).unwrap().unwrap();
        assert!(
            (i + 16.0).abs() < 0.3,
            "{format:?}: {i} LUFS after the round trip"
        );
    }
}

#[test]
fn speech_regions_on_real_speech() {
    let speech =
        resample::resample(&decode::decode_file(&fixture("say_fllr.wav")).unwrap(), SR).unwrap();
    let min_gap = (0.120 * f64::from(SR)) as usize;
    let regions = analysis::speech_regions(&speech, -45.0, min_gap);
    assert!(
        !regions.is_empty() && regions.len() <= 3,
        "\"Hi there\": {regions:?}"
    );
    let bounds = analysis::trim_bounds(&speech, -45.0, 0).unwrap();
    assert_eq!(regions.first().unwrap().start, bounds.start);
    assert_eq!(regions.last().unwrap().end, bounds.end);
    assert!(regions.windows(2).all(|w| w[1].start - w[0].end >= min_gap));
}
