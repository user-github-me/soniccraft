//! Loudness accuracy against theory and against `ffmpeg -af ebur128=peak=true` (when ffmpeg is on
//! PATH), plus the pan law's loudness preservation and the mastering loop on music-like material.

use std::path::{Path, PathBuf};

use soniccraft_dsp::master::{self, MasterParams};
use soniccraft_dsp::render::{self, ClipSpec};
use soniccraft_dsp::{AudioBuffer, decode, ffmpeg, loudness, resample, wav};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn sine(rate: u32, secs: f64, freq: f64, amp: f64) -> Vec<f32> {
    let n = (secs * f64::from(rate)).round() as usize;
    (0..n)
        .map(|i| {
            (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin()) as f32
        })
        .collect()
}

/// Deterministic music-like material: band-limited noise bursts with a slow envelope plus transients.
fn music_like(rate: u32, secs: f64, level: f32) -> AudioBuffer {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    };
    let n = (secs * f64::from(rate)) as usize;
    let (mut l, mut r) = (Vec::with_capacity(n), Vec::with_capacity(n));
    let (mut lp_l, mut lp_r, mut lp2_l, mut lp2_r) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let burst = if (t * 1.7).fract() < 0.55 { 1.0 } else { 0.2 };
        let env = level * burst * (0.7 + 0.3 * (t * 0.8).sin());
        lp_l += 0.3 * (rnd() - lp_l);
        lp_r += 0.3 * (rnd() - lp_r);
        lp2_l += 0.5 * (lp_l - lp2_l);
        lp2_r += 0.5 * (lp_r - lp2_r);
        let (mut a, mut b) = (lp2_l * env * 4.0, lp2_r * env * 4.0);
        if i % (rate as usize / 2) == 0 {
            a += 2.5 * level;
            b -= 2.0 * level;
        }
        l.push(a);
        r.push(b);
    }
    AudioBuffer::new(rate, vec![l, r])
}

/// `(integrated LUFS, true peak dBTP)` from ffmpeg's ebur128 filter (3-decimal metadata of the last
/// frame).
fn ffmpeg_r128(bin: &Path, file: &Path) -> (f64, f64) {
    let out = std::process::Command::new(bin)
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-i"])
        .arg(file)
        .args([
            "-af",
            "ebur128=peak=true:metadata=1,ametadata=mode=print:file=-",
            "-f",
            "null",
            "-",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let last = |key: &str| -> f64 {
        text.lines()
            .rev()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key}"))
            .trim()
            .parse()
            .unwrap()
    };
    let tp_linear = last("lavfi.r128.true_peak=");
    (last("lavfi.r128.I="), 20.0 * tp_linear.log10())
}

#[test]
fn sine_calibration() {
    let tone = sine(48_000, 5.0, 1000.0, 0.1); // -20 dBFS peak
    let stereo =
        loudness::measure(&AudioBuffer::new(48_000, vec![tone.clone(), tone.clone()])).unwrap();
    let mono = loudness::measure(&AudioBuffer::mono(48_000, tone)).unwrap();
    let (s, m) = (
        stereo.integrated_lufs.unwrap(),
        mono.integrated_lufs.unwrap(),
    );
    assert!(
        (s + 20.0).abs() < 0.05,
        "stereo 1 kHz at -20 dBFS: {s} LUFS"
    );
    assert!((m + 23.0).abs() < 0.05, "mono 1 kHz at -20 dBFS: {m} LUFS");
}

#[test]
fn matches_ffmpeg_ebur128() {
    let Some(bin) = ffmpeg::find_ffmpeg() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tone = sine(48_000, 5.0, 1000.0, 0.1);
    let speech = decode::decode_file(&fixture("say_fllr.wav")).unwrap();
    let speech_48k = resample::resample(&speech, 48_000).unwrap();
    let mut cases: Vec<(&str, AudioBuffer)> = vec![
        (
            "stereo_sine",
            AudioBuffer::new(48_000, vec![tone.clone(), tone.clone()]),
        ),
        ("mono_sine", AudioBuffer::mono(48_000, tone)),
        ("music_44k", music_like(44_100, 10.0, 0.2)),
        ("music_48k_hot", music_like(48_000, 6.0, 0.35)),
        ("speech_48k", speech_48k),
        (
            "tone_click_m4a",
            decode::decode_file(&fixture("tone_click_stereo44k.m4a")).unwrap(),
        ),
    ];
    // Mastered material too.
    let mut mastered = music_like(48_000, 8.0, 0.05);
    master::master(
        &mut mastered,
        &MasterParams {
            target_lufs: -14.0,
            true_peak_db: -1.0,
            normalize: true,
        },
    )
    .unwrap();
    cases.push(("mastered", mastered));

    for (name, buf) in cases {
        let path = dir.path().join(format!("{name}.wav"));
        wav::write_f32(&path, &buf).unwrap();
        let ours = loudness::measure(&buf).unwrap();
        let (i, tp) = ffmpeg_r128(&bin, &path);
        let our_i = ours.integrated_lufs.unwrap();
        let our_tp = ours.true_peak_dbtp.unwrap();
        eprintln!("{name}: I {our_i:.3} vs {i:.3} LUFS, TP {our_tp:.3} vs {tp:.3} dBTP");
        assert!(
            (our_i - i).abs() <= 0.1,
            "{name}: integrated {our_i} vs ffmpeg {i}"
        );
        assert!(
            (our_tp - tp).abs() <= 0.2,
            "{name}: true peak {our_tp} vs ffmpeg {tp}"
        );
    }
}

#[test]
fn pan_law_preserves_bs1770_loudness() {
    let speech = resample::resample(
        &decode::decode_file(&fixture("say_fllr.wav")).unwrap(),
        48_000,
    )
    .unwrap();
    let reference = loudness::integrated_lufs(&speech).unwrap().unwrap();
    for pan in [-1.0f32, -0.5, 0.0, 0.3, 1.0] {
        let mut stereo = AudioBuffer::silent(48_000, 2, speech.frames());
        render::mix_into(&mut stereo, &speech, 0, 1.0, pan).unwrap();
        let panned = loudness::integrated_lufs(&stereo).unwrap().unwrap();
        assert!(
            (panned - reference).abs() < 0.01,
            "pan {pan}: {panned} vs mono {reference}"
        );
    }
    // Clip rendering into a stereo stem goes through the same law.
    let mut stem = AudioBuffer::silent(48_000, 2, speech.frames());
    render::mix_clip(
        &mut stem,
        &speech,
        &ClipSpec::new(0, speech.frames()),
        -0.25,
    )
    .unwrap();
    assert!((loudness::integrated_lufs(&stem).unwrap().unwrap() - reference).abs() < 0.01);
}

#[test]
fn master_hits_targets_on_music_like_material() {
    for (rate, target, ceiling, level) in [
        (48_000u32, -16.0, -1.0, 0.05f32),
        (44_100, -14.0, -1.0, 0.3),
        (48_000, -23.0, -1.0, 0.02),
        (48_000, -24.0, -2.0, 0.4),
    ] {
        let mut buf = music_like(rate, 10.0, level);
        let res = master::master(
            &mut buf,
            &MasterParams {
                target_lufs: target,
                true_peak_db: ceiling,
                normalize: true,
            },
        )
        .unwrap();
        let after = loudness::measure(&buf).unwrap();
        let i = after.integrated_lufs.unwrap();
        assert!(
            (i - target).abs() <= 0.2,
            "{rate} Hz → {target}: got {i} after {} iterations",
            res.iterations
        );
        let tp = after.true_peak_dbtp.unwrap();
        assert!(
            tp <= ceiling,
            "{rate} Hz → {target}: true peak {tp} > {ceiling}"
        );
        assert_eq!(res.after.integrated_lufs, Some(i));
        assert!(res.target_reached);
    }
}
