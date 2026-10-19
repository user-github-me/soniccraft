//! Encode → decode round trips for every output format, gapless MP3 alignment and byte-level
//! determinism.

use std::path::Path;

use soniccraft_dsp::decode::{self, DecodeOptions};
use soniccraft_dsp::encode::{self, EncodeFormat, EncodeOptions};
use soniccraft_dsp::ffmpeg::{self, Ffmpeg};
use soniccraft_dsp::{AudioBuffer, DspError};

/// A quiet 330 Hz tone with a click at exactly `click_secs` (and a smaller one on the right channel).
fn test_signal(rate: u32, channels: usize, secs: f64, click_secs: f64) -> AudioBuffer {
    let n = (secs * f64::from(rate)).round() as usize;
    let click = (click_secs * f64::from(rate)).round() as usize;
    let planes = (0..channels)
        .map(|c| {
            (0..n)
                .map(|i| {
                    let t = i as f64 / f64::from(rate);
                    let mut s = 0.05
                        * (2.0 * std::f64::consts::PI * 330.0 * t).sin()
                        * if c == 0 { 1.0 } else { 0.5 };
                    if i == click {
                        s += if c == 0 { 0.9 } else { -0.6 };
                    }
                    s as f32
                })
                .collect()
        })
        .collect();
    AudioBuffer::new(rate, planes)
}

fn argmax_abs(x: &[f32]) -> usize {
    x.iter()
        .enumerate()
        .fold((0, 0.0f32), |(bi, bv), (i, v)| {
            if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
        })
        .0
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

fn opts(format: EncodeFormat, dither: bool) -> EncodeOptions {
    EncodeOptions {
        format,
        dither,
        ffmpeg: Ffmpeg::Auto,
    }
}

fn encode_decode(
    buf: &AudioBuffer,
    dir: &Path,
    name: &str,
    options: &EncodeOptions,
) -> (AudioBuffer, encode::EncodeSummary) {
    let path = dir.join(name);
    let summary =
        encode::encode_file(buf, &path, options).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        summary.bytes,
        std::fs::metadata(&path).unwrap().len(),
        "{name}: reported size"
    );
    let decoded = decode::decode_file(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    (decoded, summary)
}

#[test]
fn wav_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let buf = test_signal(48_000, 2, 1.0, 0.5);
    for (depth, dither, tolerance) in [
        (16u16, false, 0.5 / 32768.0),
        (16, true, 1.5 / 32768.0),
        (24, false, 0.5 / 8_388_608.0),
        (32, false, 0.0),
    ] {
        let (back, summary) = encode_decode(
            &buf,
            dir.path(),
            &format!("w{depth}{dither}.wav"),
            &opts(EncodeFormat::Wav { bit_depth: depth }, dither),
        );
        assert_eq!(summary.bit_depth, Some(depth));
        assert_eq!(
            (back.sample_rate, back.frames(), back.num_channels()),
            (48_000, 48_000, 2)
        );
        for c in 0..2 {
            let err = max_abs_diff(&back.channels[c], &buf.channels[c]);
            assert!(
                err <= tolerance + 1e-9,
                "wav {depth} dither {dither}: error {err}"
            );
        }
    }
}

#[test]
fn flac_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    for (rate, channels, frames) in [
        (48_000u32, 2usize, 48_000usize),
        (44_100, 1, 4096 * 3),
        (22_050, 2, 1000),
    ] {
        let buf = test_signal(rate, channels, frames as f64 / f64::from(rate), 0.02);
        for (depth, dither, tolerance) in [
            (16u16, false, 0.5 / 32768.0),
            (16, true, 1.5 / 32768.0),
            (24, false, 0.5 / 8_388_608.0),
        ] {
            let name = format!("f{rate}_{depth}{dither}.flac");
            let (back, _) = encode_decode(
                &buf,
                dir.path(),
                &name,
                &opts(EncodeFormat::Flac { bit_depth: depth }, dither),
            );
            assert_eq!(
                (back.sample_rate, back.frames(), back.num_channels()),
                (rate, frames, channels),
                "{name}"
            );
            for c in 0..channels {
                let err = max_abs_diff(&back.channels[c], &buf.channels[c]);
                assert!(err <= tolerance + 1e-9, "{name}: error {err}");
            }
        }
    }
}

#[cfg(feature = "mp3")]
#[test]
fn mp3_is_gapless_and_aligned() {
    let dir = tempfile::tempdir().unwrap();
    // (rate, channels, requested, used): 32 kbps at 16 kHz cannot carry the gapless tag → 40 kbps.
    for (rate, channels, bitrate, used) in [
        (48_000u32, 1usize, 128_000u32, 128_000u32),
        (44_100, 2, 192_000, 192_000),
        (24_000, 1, 64_000, 64_000),
        (22_050, 2, 56_000, 56_000),
        (16_000, 1, 32_000, 40_000),
        (8000, 1, 24_000, 24_000),
        (12_000, 2, 64_000, 64_000),
        (32_000, 2, 320_000, 320_000),
    ] {
        let buf = test_signal(rate, channels, 1.5, 0.5);
        let name = format!("m{rate}_{channels}.mp3");
        let (back, summary) = encode_decode(
            &buf,
            dir.path(),
            &name,
            &opts(
                EncodeFormat::Mp3 {
                    bitrate_bps: bitrate,
                },
                true,
            ),
        );
        assert_eq!(summary.bitrate_bps, Some(used), "{name}");
        assert_eq!(summary.encoder, "lame");
        assert_eq!(
            (back.sample_rate, back.num_channels()),
            (rate, channels),
            "{name}"
        );
        assert_eq!(
            back.frames(),
            buf.frames(),
            "{name}: decoded length equals the input length"
        );
        let expected = rate as usize / 2;
        assert_eq!(
            argmax_abs(&back.channels[0]),
            expected,
            "{name}: click decodes at exactly 0.5 s"
        );
        if channels == 2 {
            assert!(
                argmax_abs(&back.channels[1]).abs_diff(expected) <= 1,
                "{name}: right channel"
            );
        }
        // ffmpeg honours the same Info/LAME tag.
        if let Some(bin) = ffmpeg::find_ffmpeg() {
            let out = std::process::Command::new(bin)
                .args(["-v", "error", "-i"])
                .arg(dir.path().join(&name))
                .args(["-f", "f32le", "-c:a", "pcm_f32le", "pipe:1"])
                .output()
                .unwrap();
            assert!(out.status.success());
            let theirs = decode::pcm_f32le_to_buffer(&out.stdout, rate, channels);
            assert_eq!(theirs.frames(), buf.frames(), "{name}: ffmpeg length");
            assert_eq!(
                argmax_abs(&theirs.channels[0]),
                expected,
                "{name}: ffmpeg click position"
            );
        }
    }
}

#[cfg(feature = "mp3")]
#[test]
fn mp3_snaps_bitrates_to_valid_values() {
    let dir = tempfile::tempdir().unwrap();
    let buf = test_signal(24_000, 1, 0.3, 0.1);
    let (_, summary) = encode_decode(
        &buf,
        dir.path(),
        "snap.mp3",
        &opts(
            EncodeFormat::Mp3 {
                bitrate_bps: 320_000,
            },
            true,
        ),
    );
    assert_eq!(
        summary.bitrate_bps,
        Some(160_000),
        "MPEG-2 tops out at 160 kbps"
    );
}

#[test]
fn native_encoders_are_deterministic() {
    let buf = test_signal(44_100, 2, 2.0, 0.7);
    let mut formats = vec![
        (EncodeFormat::Wav { bit_depth: 16 }, true),
        (EncodeFormat::Wav { bit_depth: 24 }, true),
        (EncodeFormat::Wav { bit_depth: 32 }, true),
        (EncodeFormat::Flac { bit_depth: 16 }, true),
        (EncodeFormat::Flac { bit_depth: 24 }, false),
    ];
    if cfg!(feature = "mp3") {
        formats.push((
            EncodeFormat::Mp3 {
                bitrate_bps: 192_000,
            },
            true,
        ));
    }
    let dir = tempfile::tempdir().unwrap();
    for (format, dither) in formats {
        let o = opts(format, dither);
        let a = encode::encode_to_vec(&buf, &o).unwrap();
        let b = encode::encode_to_vec(&buf, &o).unwrap();
        assert_eq!(a, b, "{format:?}: two encodes are byte-identical");
        let path = dir.path().join(format!("det.{}", format.name()));
        encode::encode_file(&buf, &path, &o).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            a,
            "{format:?}: file matches the in-memory encode"
        );
    }
}

#[test]
fn ffmpeg_formats_round_trip() {
    if ffmpeg::find_ffmpeg().is_none() {
        let dir = tempfile::tempdir().unwrap();
        let err = encode::encode_file(
            &test_signal(48_000, 1, 0.5, 0.25),
            &dir.path().join("x.ogg"),
            &opts(
                EncodeFormat::Ogg {
                    bitrate_bps: 128_000,
                },
                true,
            ),
        )
        .unwrap_err();
        assert!(matches!(err, DspError::FfmpegNotFound(_)));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let buf = test_signal(44_100, 2, 2.0, 1.0);
    for (format, ext, rate) in [
        (
            EncodeFormat::Ogg {
                bitrate_bps: 160_000,
            },
            "ogg",
            44_100u32,
        ),
        (
            EncodeFormat::M4a {
                bitrate_bps: 160_000,
            },
            "m4a",
            44_100,
        ),
        (
            EncodeFormat::Opus {
                bitrate_bps: 96_000,
            },
            "opus",
            48_000,
        ),
    ] {
        let name = format!("out.{ext}");
        let (back, summary) = encode_decode(&buf, dir.path(), &name, &opts(format, true));
        assert_eq!(summary.sample_rate, rate, "{name}");
        assert!(
            summary.encoder.starts_with("ffmpeg/"),
            "{name}: {}",
            summary.encoder
        );
        assert_eq!((back.sample_rate, back.num_channels()), (rate, 2), "{name}");
        let expected_len = buf.frames() as u64 * u64::from(rate) / 44_100;
        assert!(
            back.frames().abs_diff(expected_len as usize) <= 2,
            "{name}: {} frames, expected {expected_len}",
            back.frames()
        );
        let click = argmax_abs(&back.channels[0]);
        assert!(
            click.abs_diff(rate as usize) <= 2,
            "{name}: click at {click}, expected {rate}"
        );
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }
    // Opus decodes only through the ffmpeg fallback; without it the error is explicit.
    let err = decode::decode_file_with(
        &dir.path().join("out.opus"),
        &DecodeOptions {
            ffmpeg: Ffmpeg::Disabled,
        },
    )
    .unwrap_err();
    assert!(matches!(err, DspError::Decode { .. }));
}

#[test]
fn encode_creates_parent_directories_and_replaces_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("deeper").join("out.wav");
    let buf = test_signal(16_000, 1, 0.25, 0.1);
    encode::encode_file(
        &buf,
        &path,
        &opts(EncodeFormat::Wav { bit_depth: 16 }, false),
    )
    .unwrap();
    let first = std::fs::read(&path).unwrap();
    let louder = AudioBuffer::new(
        16_000,
        vec![buf.channels[0].iter().map(|s| s * 0.5).collect()],
    );
    encode::encode_file(
        &louder,
        &path,
        &opts(EncodeFormat::Wav { bit_depth: 16 }, false),
    )
    .unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), first);
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
}

#[cfg(not(feature = "mp3"))]
#[test]
fn mp3_without_lame_goes_through_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let buf = test_signal(48_000, 2, 1.0, 0.5);
    let format = EncodeFormat::Mp3 {
        bitrate_bps: 192_000,
    };
    if ffmpeg::find_ffmpeg().is_none() {
        let err =
            encode::encode_file(&buf, &dir.path().join("x.mp3"), &opts(format, true)).unwrap_err();
        assert!(matches!(err, DspError::FfmpegNotFound(_)));
        return;
    }
    let (back, summary) = encode_decode(&buf, dir.path(), "ff.mp3", &opts(format, true));
    assert_eq!(summary.encoder, "ffmpeg/libmp3lame");
    assert_eq!(summary.bitrate_bps, Some(192_000));
    assert_eq!(
        back.frames(),
        buf.frames(),
        "ffmpeg writes the gapless Info tag too"
    );
    assert_eq!(argmax_abs(&back.channels[0]), 24_000);
    assert!(
        encode::encode_to_vec(&buf, &opts(format, true)).is_err(),
        "no in-memory MP3 without LAME"
    );
}

#[test]
fn ffmpeg_encoders_handle_awkward_file_names() {
    if ffmpeg::find_ffmpeg().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let buf = test_signal(48_000, 1, 0.5, 0.25);
    let path = dir.path().join("-out:x.ogg");
    let summary = encode::encode_file(
        &buf,
        &path,
        &opts(
            EncodeFormat::Ogg {
                bitrate_bps: 96_000,
            },
            true,
        ),
    )
    .unwrap();
    assert_eq!(summary.bytes, std::fs::metadata(&path).unwrap().len());
    assert_eq!(decode::decode_file(&path).unwrap().frames(), buf.frames());
}
