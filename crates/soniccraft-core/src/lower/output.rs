//! Lowering of the document-wide settings: `output` (DESIGN.md §2.3), `subtitles` (§2.4), `timing` (§2.5)
//! and `mix` (§2.6).

use super::LowerOptions;
use super::check::{self, Sign, TimeUse, fmt_num};
use crate::diagnostics::{self, Diagnostic, codes};
use crate::ir::{
    LoudnessPreset, MixConfig, OutputConfig, OutputFormat, OverlapPolicy, SubtitleConfig,
    SubtitleFormat, TimingConfig,
};
use crate::model::{MixSpec, OutputSpec, SubtitleSpec, TimingSpec};
use crate::values::SubtitleExport;

/// Default output sample rate.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// Default output channel count.
pub const DEFAULT_CHANNELS: u16 = 2;
/// Default PCM bit depth for wav/flac.
pub const DEFAULT_BIT_DEPTH: u16 = 24;

/// Sample rates the mp3 encoder supports.
pub const MP3_SAMPLE_RATES: &[u32] =
    &[8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

/// Constant mp3 bitrates (kbps) per MPEG version, as LAME 3.100's `bitrate_table`: MPEG-1 (32–48 kHz),
/// MPEG-2 (16–24 kHz) and MPEG-2.5 (8–12 kHz). Must match `soniccraft_dsp::encode::mp3_bitrates_kbps`.
const MP3_KBPS_MPEG1: &[u32] = &[
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const MP3_KBPS_MPEG2: &[u32] = &[8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
const MP3_KBPS_MPEG25: &[u32] = &[8, 16, 24, 32, 40, 48, 56, 64];

/// The constant mp3 bitrates (kbps) valid at `sample_rate`.
pub fn mp3_bitrates_kbps(sample_rate: u32) -> &'static [u32] {
    if sample_rate >= 32_000 {
        MP3_KBPS_MPEG1
    } else if sample_rate >= 16_000 {
        MP3_KBPS_MPEG2
    } else {
        MP3_KBPS_MPEG25
    }
}

/// `(target_lufs, true_peak_db)` of a loudness preset.
pub fn preset_targets(preset: LoudnessPreset) -> (f64, f64) {
    match preset {
        LoudnessPreset::Podcast => (-16.0, -1.0),
        LoudnessPreset::Streaming => (-14.0, -1.0),
        LoudnessPreset::Broadcast => (-23.0, -1.0),
        LoudnessPreset::Atsc => (-24.0, -2.0),
    }
}

/// Default bitrate (bits per second) of a lossy format; None for wav/flac.
pub fn default_bitrate_bps(format: OutputFormat) -> Option<u32> {
    match format {
        OutputFormat::Wav | OutputFormat::Flac => None,
        OutputFormat::Mp3 => Some(192_000),
        OutputFormat::Ogg => Some(160_000),
        OutputFormat::Opus => Some(96_000),
        OutputFormat::M4a => Some(160_000),
    }
}

/// The lowercase extension of `path`, if any.
fn extension(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// `path` with its extension replaced by `ext`.
fn with_extension(path: &str, ext: &str) -> String {
    match path.rsplit_once('.') {
        Some((stem, old)) if !old.contains(['/', '\\']) => format!("{stem}.{ext}"),
        _ => format!("{path}.{ext}"),
    }
}

/// Resolve the `output` block plus CLI/MCP overrides.
pub(super) fn output(
    diags: &mut Vec<Diagnostic>,
    spec: Option<&OutputSpec>,
    options: &LowerOptions,
) -> OutputConfig {
    let default = OutputSpec::default();
    let spec = spec.unwrap_or(&default);

    // ---- format -----------------------------------------------------------------------------------
    let doc_format = check::enum_name(
        diags,
        spec.format.as_deref(),
        "output.format",
        OutputFormat::parse,
        OutputFormat::NAMES,
        "output format",
    );
    let path = options
        .output_path_override
        .clone()
        .or_else(|| spec.path.clone());
    let path_field = if options.output_path_override.is_some() {
        None
    } else {
        Some("output.path")
    };
    if let Some(p) = &path
        && p.trim().is_empty()
    {
        let mut d = Diagnostic::error(codes::INVALID_VALUE, "the output path is empty")
            .with_hint("Set `output.path` to a file name such as `episode.mp3`, or remove it");
        if let Some(field) = path_field {
            d = d.with_path(field);
        }
        diags.push(d);
    }
    let ext = path.as_deref().and_then(extension);
    let ext_format = ext.as_deref().and_then(OutputFormat::parse);
    let explicit = options.format_override.or(doc_format);
    // An unknown `output.format` was reported above; skip checks that depend on the format.
    let format_known = !(spec.format.is_some() && explicit.is_none());
    let format = match explicit {
        Some(format) => {
            if let (Some(_), Some(p)) = (&ext, &path)
                && ext_format != Some(format)
            {
                // The CLI and MCP pass absolute override paths; diagnostics end up in manifests and previews,
                // so they show the path as written when it is relative and only the file name otherwise.
                let shown = diagnostics::display_path(p);
                let mut d = Diagnostic::warning(
                    codes::FORMAT_EXTENSION_MISMATCH,
                    format!("output format `{format}` does not match the extension of `{shown}`; the file will contain {format} data"),
                )
                .with_hint(match ext_format {
                    // The path came from the render call (CLI `-o`, MCP `output_path`): the format can come
                    // from the call too.
                    Some(other) if options.output_path_override.is_some() => format!(
                        "Rename the output to `{}`, or render {other} to match the extension: pass \
                         `--format {other}` (MCP: `format: \"{other}\"`) or set `format: {other}` in the timeline",
                        with_extension(&shown, format.as_str())
                    ),
                    Some(other) => format!(
                        "Rename the output to `{}`, or set `format: {other}` to match the extension",
                        with_extension(&shown, format.as_str())
                    ),
                    None => format!("Rename the output to `{}`", with_extension(&shown, format.as_str())),
                });
                if options.format_override.is_none() {
                    d = d.with_path("output.format");
                } else if let Some(field) = path_field {
                    d = d.with_path(field);
                }
                diags.push(d);
            }
            format
        }
        None => match (&ext, ext_format) {
            (Some(_), Some(format)) => format,
            (Some(ext), None) if format_known => {
                let mut d = Diagnostic::error(
                    codes::INVALID_VALUE,
                    format!("cannot infer the output format from the file extension `.{ext}`"),
                )
                .with_hint(
                    "Use one of the extensions .wav, .mp3, .flac, .ogg, .opus or .m4a, or set `output.format` \
                     explicitly",
                );
                if let Some(field) = path_field {
                    d = d.with_path(field);
                }
                diags.push(d);
                OutputFormat::Wav
            }
            (Some(_), None) | (None, _) => OutputFormat::Wav,
        },
    };

    // ---- sample rate / channels / bit depth ---------------------------------------------------------
    let sample_rate = match spec.sample_rate {
        None => DEFAULT_SAMPLE_RATE,
        Some(sr) if !(8_000..=192_000).contains(&sr) => {
            diags.push(
                Diagnostic::error(
                    codes::UNSUPPORTED_SAMPLE_RATE,
                    format!("sample rate {sr} Hz is outside the supported range 8000–192000 Hz"),
                )
                .with_path("output.sample_rate")
                .with_hint("Use 48000 (the default) or 44100"),
            );
            DEFAULT_SAMPLE_RATE
        }
        Some(sr)
            if format_known && format == OutputFormat::Mp3 && !MP3_SAMPLE_RATES.contains(&sr) =>
        {
            let nearest = MP3_SAMPLE_RATES
                .iter()
                .copied()
                .min_by_key(|r| r.abs_diff(sr))
                .unwrap_or(48_000);
            diags.push(
                Diagnostic::error(codes::UNSUPPORTED_SAMPLE_RATE, format!("mp3 does not support {sr} Hz"))
                    .with_path("output.sample_rate")
                    .with_hint(format!(
                        "Use `sample_rate: {nearest}` (mp3 supports {}), or choose another format such as wav or \
                         flac",
                        MP3_SAMPLE_RATES.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                    )),
            );
            nearest
        }
        Some(sr) => sr,
    };
    if format_known && format == OutputFormat::Opus && sample_rate != 48_000 {
        diags.push(
            Diagnostic::info(
                codes::UNSUPPORTED_SAMPLE_RATE,
                format!("opus files are always encoded at 48000 Hz; the mix is rendered at {sample_rate} Hz and resampled by ffmpeg"),
            )
            .with_path("output.sample_rate")
            .with_hint("Set `sample_rate: 48000` to render at the opus rate directly"),
        );
    }

    let channels = match spec.channels {
        None => DEFAULT_CHANNELS,
        Some(c @ (1 | 2)) => c,
        Some(c) => {
            diags.push(
                Diagnostic::error(
                    codes::INVALID_VALUE,
                    format!("`channels` must be 1 (mono) or 2 (stereo), got {c}"),
                )
                .with_path("output.channels")
                .with_hint("Use `channels: 2` (stereo, the default) or `channels: 1` (mono)"),
            );
            DEFAULT_CHANNELS
        }
    };

    let bit_depth = match spec.bit_depth {
        None => DEFAULT_BIT_DEPTH,
        Some(b) if !matches!(b, 16 | 24 | 32) => {
            diags.push(
                Diagnostic::error(
                    codes::UNSUPPORTED_BIT_DEPTH,
                    format!("bit depth {b} is not supported"),
                )
                .with_path("output.bit_depth")
                .with_hint("Use 16 or 24 (integer PCM), or 32 (float, wav only)"),
            );
            DEFAULT_BIT_DEPTH
        }
        Some(32) if format_known && format == OutputFormat::Flac => {
            diags.push(
                Diagnostic::error(
                    codes::UNSUPPORTED_BIT_DEPTH,
                    "flac supports 16 or 24 bits, not 32",
                )
                .with_path("output.bit_depth")
                .with_hint("Use `bit_depth: 24` for flac, or `format: wav` for 32-bit float"),
            );
            DEFAULT_BIT_DEPTH
        }
        Some(b) => b,
    };

    // ---- bitrate ----------------------------------------------------------------------------------
    let bitrate_bps = match default_bitrate_bps(format) {
        None => {
            if spec.bitrate.is_some() && format_known {
                diags.push(
                    Diagnostic::warning(
                        codes::BITRATE_IGNORED,
                        format!("`bitrate` is ignored for {format} output (lossless)"),
                    )
                    .with_path("output.bitrate")
                    .with_hint("Remove `bitrate`, or choose a lossy format such as `format: mp3`"),
                );
            }
            None
        }
        Some(default) => {
            let requested = spec.bitrate.as_ref().and_then(|b| match b.to_bps() {
                Ok(bps) => Some(bps),
                Err(reason) => {
                    diags.push(
                        Diagnostic::error(codes::INVALID_VALUE, format!("`bitrate`: {reason}"))
                            .with_path("output.bitrate")
                            .with_hint(
                                "Write the bitrate like `bitrate: \"192k\"` or `bitrate: 192000`",
                            ),
                    );
                    None
                }
            });
            let bps = requested.unwrap_or(default);
            if format == OutputFormat::Mp3 {
                let allowed = mp3_bitrates_kbps(sample_rate);
                let kbps = f64::from(bps) / 1000.0;
                // Nearest allowed value; ties go to the higher bitrate.
                let nearest = allowed
                    .iter()
                    .copied()
                    .min_by(|a, b| {
                        let da = (f64::from(*a) - kbps).abs();
                        let db = (f64::from(*b) - kbps).abs();
                        da.total_cmp(&db).then(b.cmp(a))
                    })
                    .unwrap_or(192);
                let adjusted = nearest * 1000;
                if adjusted != bps && requested.is_some() {
                    diags.push(
                        Diagnostic::info(
                            codes::BITRATE_ADJUSTED,
                            format!(
                                "mp3 bitrate {} kbps is not a constant bitrate the encoder supports at {sample_rate} Hz; \
                                 using {nearest} kbps",
                                fmt_num(kbps)
                            ),
                        )
                        .with_path("output.bitrate")
                        .with_hint(format!(
                            "Use one of {} kbps, e.g. `bitrate: \"{nearest}k\"`",
                            allowed.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                        )),
                    );
                }
                Some(adjusted)
            } else {
                Some(bps)
            }
        }
    };

    // ---- loudness ---------------------------------------------------------------------------------
    let loudness_preset = check::enum_name(
        diags,
        spec.loudness_preset.as_deref(),
        "output.loudness_preset",
        LoudnessPreset::parse,
        LoudnessPreset::NAMES,
        "loudness preset",
    )
    .unwrap_or(LoudnessPreset::Podcast);
    let (preset_lufs, preset_peak) = preset_targets(loudness_preset);
    let target_lufs =
        check::master_lufs(diags, spec.target_lufs, "output.target_lufs").unwrap_or(preset_lufs);
    let true_peak_db = check::range(
        diags,
        spec.true_peak_db,
        "output.true_peak_db",
        -9.0,
        0.0,
        "Use a true-peak ceiling between -9 and 0 dBTP, e.g. -1.0",
    )
    .unwrap_or(preset_peak);

    // ---- side files -------------------------------------------------------------------------------
    let subtitle_formats = subtitle_formats(diags, spec.export_subtitles.as_ref());
    let tail_ms = check::millis(
        diags,
        spec.tail_ms.as_ref(),
        "output.tail_ms",
        Sign::NonNegative,
    );
    let duration_ms = check::millis(
        diags,
        spec.duration_ms.as_ref(),
        "output.duration_ms",
        Sign::Positive,
    );
    if let Some(ms) = duration_ms {
        check::seconds_as_ms(
            diags,
            spec.duration_ms.as_ref(),
            ms,
            "output.duration_ms",
            TimeUse::Length,
        );
    }

    OutputConfig {
        format,
        format_explicit: explicit.is_some(),
        path,
        sample_rate,
        channels,
        bitrate_bps,
        bit_depth,
        loudness_preset,
        target_lufs,
        true_peak_db,
        normalize: spec.normalize.unwrap_or(true),
        subtitle_formats,
        export_stems: spec.export_stems.unwrap_or(false),
        export_manifest: spec.export_manifest.unwrap_or(false),
        tail_ms,
        duration_ms,
        dither: spec.dither.unwrap_or(true),
    }
}

fn subtitle_formats(
    diags: &mut Vec<Diagnostic>,
    export: Option<&SubtitleExport>,
) -> Vec<SubtitleFormat> {
    let parse_one = |diags: &mut Vec<Diagnostic>,
                     name: &str,
                     path: &str|
     -> Option<SubtitleFormat> {
        if let Some(f) = SubtitleFormat::parse(name) {
            return Some(f);
        }
        let valid = diagnostics::list_names(SubtitleFormat::NAMES.iter().copied(), 8);
        let lower = name.trim().to_ascii_lowercase();
        let hint = if matches!(
            lower.as_str(),
            "yes" | "on" | "true" | "no" | "off" | "false"
        ) {
            format!(
                "Use `export_subtitles: true` (vtt + srt) or `false` without quotes, or a list of formats from {valid}"
            )
        } else {
            match check::suggest(name, SubtitleFormat::NAMES.iter().copied()) {
                Some(s) => format!("Did you mean `{s}`? Valid formats: {valid}"),
                None => format!("Valid formats: {valid}; `true` means vtt + srt"),
            }
        };
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!("unknown subtitle format `{name}`"),
            )
            .with_path(path)
            .with_hint(hint),
        );
        None
    };
    let mut out = Vec::new();
    match export {
        None | Some(SubtitleExport::Flag(false)) => {}
        Some(SubtitleExport::Flag(true)) => out.extend([SubtitleFormat::Vtt, SubtitleFormat::Srt]),
        Some(SubtitleExport::One(name)) => {
            out.extend(parse_one(diags, name, "output.export_subtitles"))
        }
        Some(SubtitleExport::Many(names)) => {
            for (i, name) in names.iter().enumerate() {
                if let Some(f) = parse_one(diags, name, &format!("output.export_subtitles[{i}]"))
                    && !out.contains(&f)
                {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// Resolve `subtitles`. `speaker_labels` is returned separately when it was set explicitly; the automatic
/// default needs the cues (see the caller).
pub(super) fn subtitles(
    diags: &mut Vec<Diagnostic>,
    spec: Option<&SubtitleSpec>,
) -> (SubtitleConfig, Option<bool>) {
    let default = SubtitleSpec::default();
    let spec = spec.unwrap_or(&default);
    let positive =
        |diags: &mut Vec<Diagnostic>, v: Option<u32>, path: &str, default: usize| -> usize {
            match v {
                None => default,
                Some(0) => {
                    diags.push(
                        Diagnostic::error(
                            codes::INVALID_VALUE,
                            format!("`{}` must be at least 1", check::field_name(path)),
                        )
                        .with_path(path)
                        .with_hint(format!("Use a positive number (default {default})")),
                    );
                    default
                }
                Some(v) => v as usize,
            }
        };
    let max_chars_per_line = positive(
        diags,
        spec.max_chars_per_line,
        "subtitles.max_chars_per_line",
        42,
    );
    let max_lines = positive(diags, spec.max_lines, "subtitles.max_lines", 2);
    let max_duration_ms = check::millis(
        diags,
        spec.max_duration_ms.as_ref(),
        "subtitles.max_duration_ms",
        Sign::Positive,
    )
    .unwrap_or(6000.0);
    let mut min_duration_ms = check::millis(
        diags,
        spec.min_duration_ms.as_ref(),
        "subtitles.min_duration_ms",
        Sign::NonNegative,
    )
    .unwrap_or(800.0);
    if min_duration_ms > max_duration_ms {
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!(
                    "`min_duration_ms` ({} ms) is longer than `max_duration_ms` ({} ms)",
                    fmt_num(min_duration_ms),
                    fmt_num(max_duration_ms)
                ),
            )
            .with_path(if spec.min_duration_ms.is_some() {
                "subtitles.min_duration_ms"
            } else {
                "subtitles.max_duration_ms"
            })
            .with_hint(
                "Make `min_duration_ms` smaller than `max_duration_ms` (defaults: 800 and 6000)",
            ),
        );
        min_duration_ms = max_duration_ms;
    }
    (
        SubtitleConfig {
            max_chars_per_line,
            max_lines,
            max_duration_ms,
            min_duration_ms,
            speaker_labels: spec.speaker_labels.unwrap_or(false),
        },
        spec.speaker_labels,
    )
}

/// Resolve `timing`.
pub(super) fn timing(diags: &mut Vec<Diagnostic>, spec: Option<&TimingSpec>) -> TimingConfig {
    let default = TimingSpec::default();
    let spec = spec.unwrap_or(&default);
    TimingConfig {
        speech_overlap: check::enum_name(
            diags,
            spec.speech_overlap.as_deref(),
            "timing.speech_overlap",
            OverlapPolicy::parse,
            OverlapPolicy::NAMES,
            "speech overlap policy",
        )
        .unwrap_or(OverlapPolicy::Push),
        min_gap_ms: check::millis(
            diags,
            spec.min_gap_ms.as_ref(),
            "timing.min_gap_ms",
            Sign::NonNegative,
        )
        .unwrap_or(250.0),
        overlap_tolerance_ms: check::millis(
            diags,
            spec.overlap_tolerance_ms.as_ref(),
            "timing.overlap_tolerance_ms",
            Sign::NonNegative,
        )
        .unwrap_or(20.0),
    }
}

/// Resolve `mix`.
pub(super) fn mix(diags: &mut Vec<Diagnostic>, spec: Option<&MixSpec>) -> MixConfig {
    let reference = spec.and_then(|m| m.reference_lufs);
    MixConfig {
        reference_lufs: check::lufs(diags, reference, "mix.reference_lufs").unwrap_or(-20.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension("out/episode.MP3").as_deref(), Some("mp3"));
        assert_eq!(extension("episode"), None);
        assert_eq!(extension(".hidden"), None);
        assert_eq!(extension("dir.v2/episode"), None);
        assert_eq!(with_extension("a/b.wav", "mp3"), "a/b.mp3");
        assert_eq!(with_extension("a/b", "mp3"), "a/b.mp3");
    }

    #[test]
    fn mp3_bitrate_sets() {
        assert!(mp3_bitrates_kbps(48_000).contains(&320));
        assert!(!mp3_bitrates_kbps(22_050).contains(&192));
        assert_eq!(mp3_bitrates_kbps(8_000).last(), Some(&64));
    }

    #[test]
    fn presets() {
        assert_eq!(preset_targets(LoudnessPreset::Podcast), (-16.0, -1.0));
        assert_eq!(preset_targets(LoudnessPreset::Streaming), (-14.0, -1.0));
        assert_eq!(preset_targets(LoudnessPreset::Broadcast), (-23.0, -1.0));
        assert_eq!(preset_targets(LoudnessPreset::Atsc), (-24.0, -2.0));
    }
}
