//! Sources the reviewers broke the decoders with, and measuring delivered files: non-finite samples,
//! a VBR MP3 without a length header, implausible sample rates, layouts wider than stereo, and
//! `loudness::measure_file`.

use std::path::{Path, PathBuf};

use soniccraft_dsp::decode::{self, DecodeBackend, DecodeOptions};
use soniccraft_dsp::encode::{self, EncodeFormat, EncodeOptions};
use soniccraft_dsp::ffmpeg::{self, Ffmpeg};
use soniccraft_dsp::master::{self, MasterParams};
use soniccraft_dsp::{AudioBuffer, DspError, loudness, resample, wav};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

const NO_FFMPEG: DecodeOptions = DecodeOptions {
    ffmpeg: Ffmpeg::Disabled,
};

/// A WAV file: format tag 1 (integer PCM), 3 (float) or, with a channel mask, WAVE_FORMAT_EXTENSIBLE
/// with that sub-format; `data` is the interleaved sample bytes.
fn wav_file(
    tag: u16,
    channels: u16,
    rate: u32,
    bits: u16,
    mask: Option<u32>,
    data: &[u8],
) -> Vec<u8> {
    let align = channels * bits / 8;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&(if mask.is_some() { 0xFFFE } else { tag }).to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate.wrapping_mul(u32::from(align))).to_le_bytes());
    fmt.extend_from_slice(&align.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    if let Some(mask) = mask {
        fmt.extend_from_slice(&22u16.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());
        fmt.extend_from_slice(&mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT: {0000000X-0000-0010-8000-00AA00389B71}
        fmt.extend_from_slice(&tag.to_le_bytes());
        fmt.extend_from_slice(&[
            0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
        ]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn i16_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Interleaved stereo 330 Hz sine with NaN, +∞ and −∞ samples (review-dsp/nonfinite-clip-source-panic).
fn poisoned(rate: u32, secs: f64) -> (Vec<f32>, u64) {
    let n = (secs * f64::from(rate)) as usize;
    let mut out = Vec::with_capacity(2 * n);
    let mut bad = 0;
    for i in 0..n {
        let s =
            (0.3 * (2.0 * std::f64::consts::PI * 330.0 * i as f64 / f64::from(rate)).sin()) as f32;
        for c in 0..2 {
            let v = match (i + c) % 997 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                _ => s,
            };
            bad += u64::from(!v.is_finite());
            out.push(v);
        }
    }
    (out, bad)
}

fn all_finite(buffer: &AudioBuffer) -> bool {
    buffer.channels.iter().flatten().all(|s| s.is_finite())
}

/// review-dsp/nonfinite-clip-source-panic, review-tts-security/nonfinite-resample-panic: a float source
/// (or TTS cache entry) with NaN/∞ samples crashed the resampler or made the mix "silent".
#[test]
fn non_finite_samples_are_replaced_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    for rate in [44_100u32, 48_000] {
        let (samples, bad) = poisoned(rate, 6.0);
        let path = dir.path().join(format!("nan_{rate}.wav"));
        std::fs::write(&path, wav_file(3, 2, rate, 32, None, &f32_bytes(&samples))).unwrap();

        let decoded = decode::decode_file_with(&path, &NO_FFMPEG).unwrap();
        assert_eq!(decoded.info.repaired_samples, bad, "{rate} Hz");
        assert!(all_finite(&decoded.buffer));
        // The rest of the chain works: resampling to the output rate and measuring loudness.
        let out = resample::resample(&decoded.buffer, 48_000).unwrap();
        assert!(all_finite(&out));
        let lufs = loudness::integrated_lufs(&out).unwrap();
        assert!(
            lufs.is_some_and(|i| (-25.0..-10.0).contains(&i)),
            "{rate} Hz: {lufs:?}"
        );

        // The TTS cache reads with `wav::read`.
        let (cached, repaired) = wav::read_repaired(&path).unwrap();
        assert_eq!(repaired, bad);
        assert_eq!(cached, decoded.buffer);
        assert!(all_finite(&wav::read(&path).unwrap()));
        // Even unsanitized input never panics in the resampler.
        let raw = AudioBuffer::from_interleaved(rate, 2, &samples);
        assert!(all_finite(&resample::resample(&raw, 44_100).unwrap()));
    }
}

#[test]
fn non_finite_samples_through_the_ffmpeg_fallback() {
    let Some(bin) = ffmpeg::find_ffmpeg() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (samples, bad) = poisoned(44_100, 1.0);
    let src = dir.path().join("nan.wav");
    std::fs::write(&src, wav_file(3, 2, 44_100, 32, None, &f32_bytes(&samples))).unwrap();
    // symphonia is built without CAF: this file can only be read by the ffmpeg fallback.
    let caf = dir.path().join("nan.caf");
    let status = std::process::Command::new(bin)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&src)
        .args(["-c:a", "pcm_f32le", "-f", "caf"])
        .arg(&caf)
        .status()
        .unwrap();
    assert!(status.success());
    let decoded = decode::decode_file_with(&caf, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.info.backend, DecodeBackend::Ffmpeg);
    assert_eq!(decoded.info.repaired_samples, bad);
    assert!(all_finite(&decoded.buffer));
    assert_eq!(decoded.buffer.frames(), 44_100);
}

/// review-timing/vbr-mp3-truncated, review-dsp/mp3-vbr-noxing-truncated: symphonia cut a VBR MP3
/// without a Xing/Info header at a length estimated from its first frames (30 s played as 13.7 s).
#[test]
fn headerless_vbr_mp3_decodes_to_its_end() {
    let path = fixture("vbr_noxing_mono44k.mp3");
    // 78 MPEG frames of 1152 samples; without a LAME tag nothing is trimmed (ffmpeg decodes the same).
    let real = 78 * 1152;
    let decoded = decode::decode_file_with(&path, &NO_FFMPEG).unwrap();
    assert_eq!(decoded.buffer.frames(), real);
    assert_eq!(decoded.info.expected_frames, None, "no length is announced");
    assert!(
        decoded.info.warnings.is_empty(),
        "{:?}",
        decoded.info.warnings
    );
    assert!(
        decoded
            .info
            .notes
            .iter()
            .any(|n| n.contains("no length header")),
        "{:?}",
        decoded.info.notes
    );
    // The signal is still there in the last half second (the old decode stopped near 1.9 s).
    let tail = &decoded.buffer.channels[0][real - 22_050..real - 2048];
    assert!(tail.iter().any(|s| s.abs() > 0.05));
    // The plan uses the probe: it must agree with the render.
    let probe = decode::probe_file_with(&path, &NO_FFMPEG).unwrap();
    assert_eq!(
        (probe.frames, probe.sample_rate, probe.channels),
        (real as u64, 44_100, 1)
    );
    // In-memory bytes (TTS providers) take the same path.
    let bytes = decode::decode_bytes(std::fs::read(&path).unwrap(), Some("mp3")).unwrap();
    assert_eq!(bytes, decoded.buffer);
    if let Some(bin) = ffmpeg::find_ffmpeg() {
        let pcm = ffmpeg_s16(&bin, &path);
        assert_eq!(pcm.len(), real, "ffmpeg decodes the same length");
    }
}

/// A file made at test time like the reviewers' (noise, then a long quiet tone; stereo).
#[test]
fn headerless_vbr_mp3_made_by_ffmpeg() {
    let Some(bin) = ffmpeg::find_ffmpeg() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bed_vbr_noxing.mp3");
    let status = std::process::Command::new(&bin)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "anoisesrc=d=1:c=white:r=44100:a=0.5:seed=3",
        ])
        .args(["-f", "lavfi", "-i", "sine=f=220:r=44100:d=9"])
        .args([
            "-filter_complex",
            "[0][1]concat=n=2:v=0:a=1,aformat=channel_layouts=stereo",
        ])
        .args(["-c:a", "libmp3lame", "-q:a", "2", "-write_xing", "0"])
        .arg(&path)
        .status()
        .unwrap();
    if !status.success() {
        return; // an ffmpeg without libmp3lame
    }
    let want = ffmpeg_s16(&bin, &path).len() / 2;
    let decoded = decode::decode_file_with(&path, &NO_FFMPEG).unwrap();
    assert_eq!(decoded.buffer.frames(), want);
    assert_eq!(decoded.buffer.num_channels(), 2);
    let probe = decode::probe_file_with(&path, &NO_FFMPEG).unwrap();
    assert_eq!(probe.frames, want as u64);
}

/// ffmpeg's s16 decode of `path` (interleaved).
fn ffmpeg_s16(bin: &Path, path: &Path) -> Vec<i16> {
    let out = std::process::Command::new(bin)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let (pairs, _) = out.stdout.as_chunks::<2>();
    pairs.iter().map(|&b| i16::from_le_bytes(b)).collect()
}

/// review-dsp/pathological-source-rate-memory: a 244-byte WAV claiming 4 000 037 Hz needed 708 MB to
/// resample, 1 Hz needed 1.8 GB.
#[test]
fn implausible_sample_rates_are_rejected_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let data = i16_bytes(&[1000; 100]);
    for rate in [1u32, 999, 768_001, 4_000_037, u32::MAX] {
        let path = dir.path().join(format!("rate_{rate}.wav"));
        std::fs::write(&path, wav_file(1, 1, rate, 16, None, &data)).unwrap();
        for err in [
            decode::probe_file_with(&path, &NO_FFMPEG).unwrap_err(),
            decode::decode_file_with(&path, &DecodeOptions::default()).unwrap_err(),
        ] {
            let message = err.to_string();
            assert!(matches!(err, DspError::Decode { .. }), "{message}");
            assert!(
                message.contains(&format!("unsupported sample rate {rate} Hz"))
                    && message.contains("between 1000 and 768000 Hz"),
                "{message}"
            );
            // The engine picks its hint by the reason's words: this is not an ffmpeg problem.
            assert!(!message.contains("ffmpeg"), "{message}");
        }
    }
    // The limits themselves are fine, and resampling from them is cheap whatever the ratio.
    for rate in [1000u32, 44_101, 767_999, 768_000] {
        let path = dir.path().join(format!("ok_{rate}.wav"));
        std::fs::write(&path, wav_file(1, 1, rate, 16, None, &data)).unwrap();
        let buffer = decode::decode_file(&path).unwrap();
        assert_eq!(buffer.sample_rate, rate);
        let out = resample::resample(&buffer, 48_000).unwrap();
        assert_eq!(out.frames(), resample::output_len(100, rate, 48_000));
    }
}

/// 5.1 frames `L R C LFE Ls Rs`, each channel a distinct constant (as 16-bit samples).
fn five_one(frames: usize, values: [i16; 6]) -> Vec<u8> {
    i16_bytes(&(0..frames).flat_map(|_| values).collect::<Vec<_>>())
}

/// review-dsp/multichannel-source-centre-dropped: wider sources kept their first two channels, so a
/// 5.1 file with its content in the centre rendered as silence.
#[test]
fn wide_sources_are_mixed_down_with_bs775() {
    let s = std::f32::consts::FRAC_1_SQRT_2;
    let q = |v: i16| f32::from(v) / 32768.0;
    let values = [1000i16, 2000, 4000, 30_000, 8000, 16_000];
    let lo = q(1000) + s * q(4000) + s * q(8000);
    let ro = q(2000) + s * q(4000) + s * q(16_000);
    let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
    let dir = tempfile::tempdir().unwrap();
    // A plain PCM header (no mask): the WAVE speaker order is assumed.
    let plain = dir.path().join("plain51.wav");
    std::fs::write(
        &plain,
        wav_file(1, 6, 48_000, 16, None, &five_one(4800, values)),
    )
    .unwrap();
    // WAVE_FORMAT_EXTENSIBLE with the 5.1 (side) mask FL FR FC LFE SL SR.
    let masked = dir.path().join("masked51.wav");
    std::fs::write(
        &masked,
        wav_file(1, 6, 48_000, 16, Some(0x60F), &five_one(4800, values)),
    )
    .unwrap();
    for (path, layout, from_file) in [
        (&plain, "FL FR FC LFE BL BR", false),
        (&masked, "FL FR FC LFE SL SR", true),
    ] {
        let decoded = decode::decode_file_with(path, &NO_FFMPEG).unwrap();
        let b = &decoded.buffer;
        assert_eq!(b.num_channels(), 2);
        assert!(
            close(b.channels[0][100], lo) && close(b.channels[1][100], ro),
            "{path:?}"
        );
        let downmix = decoded.info.downmix.expect("a downmix is reported");
        assert_eq!(downmix.source_channels, 6);
        assert_eq!(downmix.layout, layout);
        assert_eq!(downmix.layout_from_file, from_file);
        assert_eq!(downmix.dropped, vec!["LFE"]);
        // Probing reports the file's own channel count.
        assert_eq!(
            decode::probe_file_with(path, &NO_FFMPEG).unwrap().channels,
            6
        );
    }
    // The reviewers' file: content only in the centre. Not silent, and mono gets the same mix.
    let centre = dir.path().join("centre51.wav");
    std::fs::write(
        &centre,
        wav_file(
            1,
            6,
            48_000,
            16,
            None,
            &five_one(4800, [0, 0, 16_384, 0, 0, 0]),
        ),
    )
    .unwrap();
    let b = decode::decode_file(&centre).unwrap();
    assert!(close(b.channels[0][7], 0.5 * s) && close(b.channels[1][7], 0.5 * s));
    assert!(close(b.to_mono().channels[0][7], 0.5 * s));
    // Mono and stereo files are untouched.
    let stereo = dir.path().join("stereo.wav");
    std::fs::write(
        &stereo,
        wav_file(1, 2, 48_000, 16, None, &i16_bytes(&[100, -200, 300, -400])),
    )
    .unwrap();
    let decoded = decode::decode_file_with(&stereo, &NO_FFMPEG).unwrap();
    assert_eq!(decoded.info.downmix, None);
    assert_eq!(
        decoded.buffer.channels,
        vec![vec![q(100), q(300)], vec![q(-200), q(-400)]]
    );
}

#[test]
fn wide_flac_uses_the_codec_layout() {
    let Some(bin) = ffmpeg::find_ffmpeg() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src51.wav");
    std::fs::write(
        &src,
        wav_file(
            1,
            6,
            48_000,
            16,
            Some(0x60F),
            &five_one(4800, [0, 0, 16_384, 0, 0, 0]),
        ),
    )
    .unwrap();
    let flac = dir.path().join("centre51.flac");
    let status = std::process::Command::new(bin)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&src)
        .args(["-c:a", "flac"])
        .arg(&flac)
        .status()
        .unwrap();
    assert!(status.success());
    let decoded = decode::decode_file_with(&flac, &NO_FFMPEG).unwrap();
    assert_eq!(decoded.info.backend, DecodeBackend::Symphonia);
    let downmix = decoded.info.downmix.expect("downmixed");
    assert_eq!(downmix.source_channels, 6);
    assert!(
        downmix.layout_from_file && downmix.layout.contains("FC"),
        "{downmix:?}"
    );
    let b = &decoded.buffer;
    let s = std::f32::consts::FRAC_1_SQRT_2;
    assert!((b.channels[0][7] - 0.5 * s).abs() < 1e-6 && (b.channels[1][7] - 0.5 * s).abs() < 1e-6);
}

/// `measure_file` reads exactly what the full decode reads, without keeping it.
#[test]
fn measuring_a_file_matches_measuring_its_decode() {
    let mut names = vec![
        "tone_click_mono48k.mp3",
        "tone_click_mono48k.flac",
        "tone_click_mono48k.aiff",
        "tone_click_stereo44k.ogg",
        "tone_click_stereo44k.m4a",
        "vbr_noxing_mono44k.mp3",
        "say_fllr.wav",
    ];
    if ffmpeg::find_ffmpeg().is_some() {
        names.push("tone_click_mono48k.opus");
    }
    for name in names {
        let path = fixture(name);
        let decoded = decode::decode_file_with(&path, &DecodeOptions::default()).unwrap();
        let want = loudness::measure(&decoded.buffer).unwrap();
        let got = loudness::measure_file(&path, &Ffmpeg::Auto).unwrap();
        assert_eq!(got.integrated_lufs, want.integrated_lufs, "{name}");
        assert_eq!(got.true_peak_dbtp, want.true_peak_dbtp, "{name}");
        assert_eq!(got.sample_peak_dbfs, want.sample_peak_dbfs, "{name}");
        assert_eq!(got.loudness_range_lu, want.loudness_range_lu, "{name}");
        assert_eq!(got.frames, decoded.buffer.frames() as u64, "{name}");
        assert_eq!(got.sample_rate, decoded.buffer.sample_rate, "{name}");
        assert_eq!(got.channels, decoded.buffer.num_channels(), "{name}");
        let backend = if name.ends_with(".opus") {
            DecodeBackend::Ffmpeg
        } else {
            DecodeBackend::Symphonia
        };
        assert_eq!(got.backend, backend, "{name}");
    }
    // Opus needs ffmpeg; the error says what was tried.
    let err =
        loudness::measure_file(&fixture("tone_click_mono48k.opus"), &Ffmpeg::Disabled).unwrap_err();
    assert!(matches!(err, DspError::Decode { .. }), "{err}");
    assert!(matches!(
        loudness::measure_file(Path::new("/definitely/not/here.mp3"), &Ffmpeg::Disabled),
        Err(DspError::Io { .. })
    ));
    let cancelled = loudness::measure_file_with_cancel(
        &fixture("tone_click_mono48k.mp3"),
        &Ffmpeg::Disabled,
        &|| true,
    );
    assert!(matches!(cancelled, Err(DspError::Cancelled)));
}

/// e2e-audio/lossy-report-pcm-truepeak: the report described the master before encoding. The engine
/// re-measures the delivered file with `measure_file` and, when a codec overshoots the ceiling,
/// re-limits the master lower (`limit_in_place`) and encodes again.
#[test]
fn delivered_lossy_files_can_be_measured_and_brought_under_the_ceiling() {
    let rate = 48_000;
    let n = 4 * rate as usize;
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut noise = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 40) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    // Music-like: low-passed noise (most energy below a few kHz, as codecs expect) with transients.
    let (mut lp_l, mut lp_r) = (0.0f32, 0.0f32);
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        lp_l += 0.2 * (noise() - lp_l);
        lp_r += 0.2 * (noise() - lp_r);
        l.push(0.5 * lp_l + if i % 4801 == 0 { 0.6 } else { 0.0 });
        r.push(0.5 * lp_r - if i % 4801 == 0 { 0.5 } else { 0.0 });
    }
    let mut master_buffer = AudioBuffer::new(rate, vec![l, r]);
    let params = MasterParams {
        target_lufs: -9.0,
        true_peak_db: -1.0,
        normalize: true,
    };
    let mastered = master::master(&mut master_buffer, &params).unwrap();
    let master_lufs = mastered.after.integrated_lufs.unwrap();
    assert!(mastered.after.true_peak_dbtp.unwrap() <= -1.0);
    let dir = tempfile::tempdir().unwrap();
    let mut formats = Vec::new();
    if cfg!(feature = "mp3") {
        formats.push(EncodeFormat::Mp3 {
            bitrate_bps: 128_000,
        });
    }
    if ffmpeg::find_ffmpeg().is_some() {
        formats.extend([
            EncodeFormat::Opus {
                bitrate_bps: 96_000,
            },
            EncodeFormat::M4a {
                bitrate_bps: 128_000,
            },
            EncodeFormat::Ogg {
                bitrate_bps: 160_000,
            },
        ]);
    }
    for format in formats {
        let path = dir.path().join(format!("hot.{}", format.name()));
        let options = EncodeOptions::new(format);
        encode::encode_file(&master_buffer, &path, &options).unwrap();
        let first = loudness::measure_file(&path, &Ffmpeg::Auto).unwrap();
        let tp = first.true_peak_dbtp.unwrap();
        assert!(
            (first.integrated_lufs.unwrap() - master_lufs).abs() < 1.0,
            "{format:?}: {first:?} vs a {master_lufs} LUFS master"
        );
        assert_eq!(first.channels, 2);
        // The compensation loop: lower the ceiling by the overshoot and encode again.
        let mut ceiling = params.true_peak_db;
        let mut measured = first;
        for _ in 0..3 {
            let over = measured.true_peak_dbtp.unwrap() - params.true_peak_db;
            if over <= 0.0 {
                break;
            }
            ceiling -= over + 0.1;
            let mut lowered = master_buffer.clone();
            soniccraft_dsp::limiter::limit_in_place(&mut lowered, 1.0, ceiling, &|| false).unwrap();
            encode::encode_file(&lowered, &path, &options).unwrap();
            measured = loudness::measure_file(&path, &Ffmpeg::Auto).unwrap();
        }
        eprintln!(
            "{}: first {tp:.2} dBTP, after {:.2} dBTP (ceiling {ceiling:.2})",
            format.name(),
            measured.true_peak_dbtp.unwrap()
        );
        assert!(
            measured.true_peak_dbtp.unwrap() <= params.true_peak_db + 0.05,
            "{format:?}: {measured:?}"
        );
    }
}
