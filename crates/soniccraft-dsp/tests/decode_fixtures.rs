//! Decoding real files: WAV quirks from TTS engines and every supported codec (see
//! `tests/fixtures/README.md`).

use std::path::{Path, PathBuf};

use soniccraft_dsp::decode::{self, DecodeBackend, DecodeOptions};
use soniccraft_dsp::ffmpeg::{self, Ffmpeg};
use soniccraft_dsp::{AudioBuffer, DspError, wav};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

const NO_FFMPEG: DecodeOptions = DecodeOptions {
    ffmpeg: Ffmpeg::Disabled,
};

/// The reference signal of `gen_ref.py`, quantized to 16-bit like the source WAV.
fn reference(sample_rate: u32) -> Vec<f32> {
    let n = sample_rate as usize / 2;
    let click = sample_rate as usize / 4;
    (0..n)
        .map(|i| {
            let mut s = 0.1
                * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / f64::from(sample_rate)).sin();
            if i == click {
                s += 0.8;
            }
            ((s * 32768.0).round().clamp(-32768.0, 32767.0) / 32768.0) as f32
        })
        .collect()
}

fn argmax_abs(x: &[f32]) -> usize {
    x.iter()
        .enumerate()
        .fold((0, 0.0f32), |(bi, bv), (i, v)| {
            if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
        })
        .0
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

#[test]
fn macos_say_wav_with_junk_and_fllr_chunks() {
    let path = fixture("say_fllr.wav");
    let buf = wav::read(&path).unwrap();
    assert_eq!(
        (buf.sample_rate, buf.num_channels(), buf.frames()),
        (16_000, 1, 12_014)
    );
    // Audio starts at byte 4096: the first samples are 0x0021, 0x0028 (not header bytes).
    assert_eq!(buf.channels[0][..2], [33.0 / 32768.0, 40.0 / 32768.0]);
    let decoded = decode::decode_file_with(&path, &NO_FFMPEG).unwrap();
    assert_eq!(decoded.info.backend, DecodeBackend::Wav);
    assert_eq!(decoded.buffer, buf);
    assert_eq!(decode::probe_file(&path).unwrap(), (12_014, 16_000, 1));
}

#[test]
fn streaming_wavs_with_unknown_sizes() {
    let piped = wav::read(&fixture("piped_list_ffff.wav")).unwrap();
    assert_eq!(
        (piped.sample_rate, piped.num_channels(), piped.frames()),
        (24_000, 1, 6000)
    );
    assert_eq!(
        piped.channels[0][..3],
        [0.0, 470.0 / 32768.0, 935.0 / 32768.0]
    );

    let ffff = wav::read(&fixture("data_ffff.wav")).unwrap();
    let zero = wav::read(&fixture("data_zero.wav")).unwrap();
    assert_eq!(
        (ffff.sample_rate, ffff.num_channels(), ffff.frames()),
        (24_000, 1, 6000)
    );
    assert_eq!(ffff, zero, "same audio, different size conventions");
    assert!(ffff.peak() > 0.1);

    for name in ["piped_list_ffff.wav", "data_ffff.wav", "data_zero.wav"] {
        let path = fixture(name);
        assert_eq!(
            decode::probe_file(&path).unwrap(),
            (6000, 24_000, 1),
            "{name}"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            decode::decode_bytes(bytes, None).unwrap(),
            wav::read(&path).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn wav_fixtures_match_ffmpeg() {
    let Some(bin) = ffmpeg::find_ffmpeg() else {
        return;
    };
    for name in [
        "say_fllr.wav",
        "piped_list_ffff.wav",
        "data_ffff.wav",
        "data_zero.wav",
    ] {
        let path = fixture(name);
        let out = std::process::Command::new(&bin)
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "f32le", "-c:a", "pcm_f32le", "pipe:1"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{name}");
        let theirs = decode::pcm_f32le_to_buffer(&out.stdout, 1, 1);
        let ours = wav::read(&path).unwrap();
        assert_eq!(ours.channels[0], theirs.channels[0], "{name}");
    }
}

#[test]
fn lossless_formats_decode_bit_exactly() {
    let expected = reference(48_000);
    for name in ["tone_click_mono48k.flac", "tone_click_mono48k.aiff"] {
        let path = fixture(name);
        let d = decode::decode_file_with(&path, &NO_FFMPEG).unwrap();
        assert_eq!(d.info.backend, DecodeBackend::Symphonia, "{name}");
        assert!(d.info.warnings.is_empty(), "{name}: {:?}", d.info.warnings);
        assert_eq!(
            (d.buffer.sample_rate, d.buffer.num_channels()),
            (48_000, 1),
            "{name}"
        );
        assert_eq!(d.buffer.channels[0], expected, "{name}");
        assert_eq!(
            decode::probe_file(&path).unwrap(),
            (24_000, 48_000, 1),
            "{name}"
        );
    }
}

/// Lossy codecs: exact length (gapless / edit list) and the click exactly where it was encoded.
fn check_lossy(
    name: &str,
    sample_rate: u32,
    channels: usize,
    options: &DecodeOptions,
) -> AudioBuffer {
    let path = fixture(name);
    let d = decode::decode_file_with(&path, options).unwrap_or_else(|e| panic!("{name}: {e}"));
    let buf = d.buffer;
    assert_eq!(
        (buf.sample_rate, buf.num_channels()),
        (sample_rate, channels),
        "{name}"
    );
    assert_eq!(
        buf.frames(),
        sample_rate as usize / 2,
        "{name}: gapless length"
    );
    let click = argmax_abs(&buf.channels[0]);
    let expected = sample_rate as usize / 4;
    assert!(
        click.abs_diff(expected) <= 1,
        "{name}: click at {click}, expected {expected}"
    );
    if channels == 2 {
        let ratio = rms(&buf.channels[1]) / rms(&buf.channels[0]);
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "{name}: channel order (R/L = {ratio})"
        );
    }
    let probed = decode::probe_file_with(&path, options).unwrap();
    assert_eq!(
        (probed.frames, probed.sample_rate, probed.channels),
        (buf.frames() as u64, sample_rate, channels),
        "{name}: probe agrees with decode"
    );
    buf
}

#[test]
fn mp3_decodes_gapless() {
    check_lossy("tone_click_mono48k.mp3", 48_000, 1, &NO_FFMPEG);
}

#[test]
fn ogg_vorbis_decodes_gapless() {
    check_lossy("tone_click_stereo44k.ogg", 44_100, 2, &NO_FFMPEG);
}

#[test]
fn m4a_edit_list_is_applied() {
    let buf = check_lossy("tone_click_stereo44k.m4a", 44_100, 2, &NO_FFMPEG);
    // Without the edit list the click would sit 1024 frames late.
    assert!(argmax_abs(&buf.channels[1]).abs_diff(11_025) <= 1);
}

#[test]
fn m4a_matches_ffmpeg_decode() {
    let Some(bin) = ffmpeg::find_ffmpeg() else {
        return;
    };
    let path = fixture("tone_click_stereo44k.m4a");
    let ours = decode::decode_file_with(&path, &NO_FFMPEG).unwrap().buffer;
    let out = std::process::Command::new(bin)
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "f32le", "-c:a", "pcm_f32le", "pipe:1"])
        .output()
        .unwrap();
    let theirs = decode::pcm_f32le_to_buffer(&out.stdout, 44_100, 2);
    assert_eq!(theirs.frames(), ours.frames());
    let max_diff = ours.channels[0]
        .iter()
        .zip(&theirs.channels[0])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_diff < 1e-3,
        "symphonia+elst vs ffmpeg max diff {max_diff}"
    );
}

#[test]
fn opus_requires_the_ffmpeg_fallback() {
    let path = fixture("tone_click_mono48k.opus");
    let err = decode::decode_file_with(&path, &NO_FFMPEG).unwrap_err();
    assert!(matches!(err, DspError::Decode { .. }), "{err}");
    if ffmpeg::find_ffmpeg().is_none() {
        return;
    }
    let buf = check_lossy(
        "tone_click_mono48k.opus",
        48_000,
        1,
        &DecodeOptions::default(),
    );
    assert!(buf.peak() > 0.5);
    let d = decode::decode_file(&path).unwrap();
    assert_eq!(d, buf);
    // in-memory bytes go through a scratch file
    let from_bytes = decode::decode_bytes(std::fs::read(&path).unwrap(), Some("opus")).unwrap();
    assert_eq!(from_bytes, buf);
    let info = decode::decode_file_with(&path, &DecodeOptions::default())
        .unwrap()
        .info;
    assert_eq!(info.backend, DecodeBackend::Ffmpeg);
}

#[test]
fn decode_bytes_probes_without_a_hint() {
    for (name, sr) in [
        ("tone_click_mono48k.mp3", 48_000),
        ("tone_click_mono48k.flac", 48_000),
        ("tone_click_stereo44k.ogg", 44_100),
    ] {
        let bytes = std::fs::read(fixture(name)).unwrap();
        let a = decode::decode_bytes(bytes.clone(), None).unwrap();
        let b = decode::decode_bytes(bytes, Some(name.rsplit('.').next().unwrap())).unwrap();
        assert_eq!(a, b, "{name}");
        assert_eq!(a.sample_rate, sr);
    }
}

#[test]
fn truncated_mp3_keeps_what_decodes_and_warns() {
    let bytes = std::fs::read(fixture("tone_click_mono48k.mp3")).unwrap();
    let cut = &bytes[..bytes.len() * 6 / 10];
    let d = decode::decode_bytes_with(cut, Some("mp3"), &NO_FFMPEG).unwrap();
    assert!(
        d.buffer.frames() < 24_000 && d.buffer.frames() > 5_000,
        "{}",
        d.buffer.frames()
    );
    assert!(
        d.info.warnings.iter().any(|w| w.contains("ended early")),
        "{:?}",
        d.info.warnings
    );
}

#[test]
fn truncated_wav_with_promised_size_uses_remaining_bytes() {
    let bytes = std::fs::read(fixture("say_fllr.wav")).unwrap();
    let cut = &bytes[..4096 + 1001]; // 500 whole frames + 1 stray byte
    let buf = decode::decode_bytes(cut.to_vec(), Some("wav")).unwrap();
    assert_eq!(buf.frames(), 500);
}

#[test]
fn garbage_is_a_decode_error_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake.mp3");
    std::fs::write(&path, b"definitely not an mp3 file, just some text").unwrap();
    let err = decode::decode_file_with(&path, &NO_FFMPEG).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("fake.mp3"), "{msg}");
    if ffmpeg::find_ffmpeg().is_some() {
        let err = decode::decode_file(&path).unwrap_err().to_string();
        assert!(err.contains("ffmpeg fallback also failed"), "{err}");
    }
}

#[test]
fn ffmpeg_fallback_handles_awkward_file_names() {
    if ffmpeg::find_ffmpeg().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // A leading dash and a `proto:`-like prefix must not be interpreted by ffmpeg.
    let path = dir.path().join("-tricky:name.opus");
    std::fs::copy(fixture("tone_click_mono48k.opus"), &path).unwrap();
    let buf = decode::decode_file(&path).unwrap();
    assert_eq!((buf.sample_rate, buf.frames()), (48_000, 24_000));
}

#[test]
fn corrupted_files_never_panic() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for name in [
        "tone_click_mono48k.mp3",
        "tone_click_mono48k.flac",
        "tone_click_mono48k.aiff",
        "tone_click_stereo44k.ogg",
        "tone_click_stereo44k.m4a",
        "say_fllr.wav",
    ] {
        let good = std::fs::read(fixture(name)).unwrap();
        let ext = name.rsplit('.').next();
        for round in 0..60 {
            let mut bad = good.clone();
            if round % 3 == 0 {
                bad.truncate((next() as usize) % good.len());
            }
            for _ in 0..1 + round % 8 {
                if bad.is_empty() {
                    break;
                }
                let at = (next() as usize) % bad.len();
                bad[at] = next() as u8;
            }
            // Ok or Err are both fine; panics and hangs are not.
            let _ = decode::decode_bytes_with(&bad, ext, &NO_FFMPEG);
        }
    }
}
