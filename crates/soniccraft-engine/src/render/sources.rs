//! Clip sources (DESIGN.md §4.2–§4.3): each distinct file is decoded once, resampled to the output rate,
//! adapted to the mix layout and measured once for source-level normalization.
//!
//! Decoding repairs what it can and says so: non-finite samples (NaN, ±∞) become silence
//! (`audio_repaired`), sources wider than stereo are mixed down with ITU-R BS.775 (`channels_downmixed`),
//! and sample rates outside 1–768 kHz are rejected (`decode_failed`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::codes;
use soniccraft_core::ir::{NodeId, Program};
use soniccraft_dsp::decode::{DecodeOptions, decode_file_with};
use soniccraft_dsp::downmix::Downmix;
use soniccraft_dsp::ffmpeg::Ffmpeg;
use soniccraft_dsp::{AudioBuffer, DspError, loudness, resample};

use crate::cancel::{Cancel, Stop};
use crate::checks::unreadable_hint;

/// A decoded clip source.
#[derive(Debug)]
pub(crate) struct Source {
    /// At the output rate; mono stays mono, wider than stereo was mixed down to stereo by the decoder,
    /// and everything is mixed down for mono output.
    pub buffer: AudioBuffer,
    /// Integrated loudness of the whole adapted source (None when unmeasurable or not needed).
    pub lufs: Option<f64>,
}

/// Every decoded source, by absolute path.
#[derive(Debug, Default)]
pub(crate) struct SourceBank {
    sources: HashMap<PathBuf, Source>,
}

impl SourceBank {
    /// The source decoded from `path`.
    pub(crate) fn get(&self, path: &Path) -> Option<&Source> {
        self.sources.get(path)
    }

    /// Whole length of every source at the output rate, by path.
    pub(crate) fn frames(&self) -> HashMap<PathBuf, u64> {
        self.sources
            .iter()
            .map(|(path, s)| (path.clone(), s.buffer.frames() as u64))
            .collect()
    }

    /// Σ frames × channels of the sources (what they hold in memory, in samples).
    pub(crate) fn samples(&self) -> u64 {
        self.sources
            .values()
            .map(|s| (s.buffer.frames() * s.buffer.num_channels()) as u64)
            .sum()
    }
}

/// Decode every clip source of `program`. Failures become `decode_failed` errors on every clip using the
/// file; decoder warnings (skipped packets, truncated streams) become `decode_failed` warnings, repaired
/// samples `audio_repaired` warnings and downmixes `channels_downmixed` notes.
pub(crate) fn decode_all(
    program: &Program,
    ffmpeg: Ffmpeg,
    cancel: &Cancel,
) -> (Result<SourceBank, Stop>, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let sample_rate = program.output.sample_rate;
    let channels = usize::from(program.output.channels);
    let options = DecodeOptions { ffmpeg };

    // Distinct sources in order of first use, with the clips that use them.
    let mut order: Vec<PathBuf> = Vec::new();
    let mut users: HashMap<PathBuf, Vec<NodeId>> = HashMap::new();
    for (id, _, clip) in program.clips() {
        users
            .entry(clip.source_path.clone())
            .or_insert_with(|| {
                order.push(clip.source_path.clone());
                Vec::new()
            })
            .push(id);
    }

    let mut bank = SourceBank::default();
    let mut failed = 0usize;
    for path in order {
        if let Err(stop) = cancel.check() {
            return (Err(stop), diagnostics);
        }
        let clips = &users[&path];
        let written = program.nodes[clips[0]]
            .as_clip()
            .map(|c| c.source.clone())
            .unwrap_or_else(|| path.display().to_string());
        let field = format!("{}.source", program.nodes[clips[0]].path);
        let normalized = clips.iter().any(|&id| {
            program
                .tracks
                .get(program.nodes[id].track)
                .is_some_and(|t| t.normalize_lufs.is_some())
        });
        match load(&path, &written, &options, sample_rate, channels, normalized) {
            Ok(loaded) => {
                let related = clip_ids(program, clips);
                for warning in &loaded.warnings {
                    diagnostics.push(
                        Diagnostic::warning(
                            codes::DECODE_FAILED,
                            format!("`{written}` decoded with problems: {warning}"),
                        )
                        .with_path(field.clone())
                        .with_hint(
                            "Re-export the file, or convert it to WAV or FLAC (e.g. `ffmpeg -i input output.wav`)",
                        )
                        .with_related(related.iter().cloned()),
                    );
                }
                if loaded.repaired > 0 {
                    diagnostics.push(repaired(&written, loaded.repaired, &field, &related));
                }
                if let Some(downmix) = &loaded.downmix {
                    diagnostics.push(downmixed(&written, downmix, channels, &field, &related));
                }
                for note in &loaded.notes {
                    tracing::info!("{written}: {note}");
                }
                bank.sources.insert(path, loaded.source);
            }
            Err(error) => {
                failed += 1;
                // Decoder messages may name the absolute path; reports only ever show the path as written.
                let reason = error
                    .to_string()
                    .replace(&path.display().to_string(), &written);
                let hint = match &error {
                    DspError::FfmpegNotFound(_) => {
                        "Install ffmpeg (e.g. `brew install ffmpeg` or `apt install ffmpeg`) to decode this \
                         format, or convert the file to WAV, FLAC or MP3"
                            .to_string()
                    }
                    _ => unreadable_hint(&reason),
                };
                for &id in clips {
                    let node = &program.nodes[id];
                    diagnostics.push(
                        Diagnostic::error(
                            codes::DECODE_FAILED,
                            format!("cannot decode `{written}` (clip `{}`): {reason}", node.id),
                        )
                        .with_path(format!("{}.source", node.path))
                        .with_hint(hint.clone())
                        .with_related([node.id.clone()]),
                    );
                }
            }
        }
    }
    if failed > 0 {
        let plural = if failed == 1 { "" } else { "s" };
        return (
            Err(Stop::failed(format!(
                "{failed} audio source{plural} could not be decoded"
            ))),
            diagnostics,
        );
    }
    (Ok(bank), diagnostics)
}

/// `audio_repaired` (warning) for a source whose non-finite samples were replaced with silence.
fn repaired(written: &str, count: u64, field: &str, related: &[String]) -> Diagnostic {
    let name = Path::new(written)
        .file_name()
        .map_or_else(|| written.to_string(), |n| n.to_string_lossy().into_owned());
    Diagnostic::warning(
        codes::AUDIO_REPAIRED,
        format!(
            "`{written}` contained {count} non-finite sample{} (NaN/±∞); {} replaced with silence",
            if count == 1 { "" } else { "s" },
            if count == 1 { "it was" } else { "they were" }
        ),
    )
    .with_path(field)
    .with_hint(format!(
        "Re-export the file as 16- or 24-bit PCM (e.g. `ffmpeg -i {name} -c:a pcm_s24le fixed.wav`) and point \
         `source` at it"
    ))
    .with_related(related.iter().cloned())
}

/// `channels_downmixed` (info) for a source wider than stereo.
fn downmixed(
    written: &str,
    downmix: &Downmix,
    channels: usize,
    field: &str,
    related: &[String],
) -> Diagnostic {
    let dropped = if downmix.dropped.is_empty() {
        String::new()
    } else {
        format!(" ({} dropped)", downmix.dropped.join(", "))
    };
    let assumed = if downmix.layout_from_file {
        ""
    } else {
        ", assuming the WAVE channel order"
    };
    let target = if channels == 1 {
        "stereo with ITU-R BS.775 and then to mono"
    } else {
        "stereo with ITU-R BS.775"
    };
    Diagnostic::info(
        codes::CHANNELS_DOWNMIXED,
        format!(
            "`{written}` has {} channels ({}); it was mixed down to {target}{assumed}{dropped}",
            downmix.source_channels, downmix.layout
        ),
    )
    .with_path(field)
    .with_hint(
        "Nothing needs to change; to control the downmix yourself, export a stereo file (e.g. `ffmpeg -i in.wav \
         -ac 2 out.wav`)",
    )
    .with_related(related.iter().cloned())
}

fn clip_ids(program: &Program, clips: &[NodeId]) -> Vec<String> {
    clips
        .iter()
        .map(|&id| program.nodes[id].id.clone())
        .collect()
}

/// A loaded source and what decoding it said.
struct Loaded {
    source: Source,
    warnings: Vec<String>,
    repaired: u64,
    downmix: Option<Downmix>,
    notes: Vec<String>,
}

/// Decode, resample, adapt and (when `measure`) measure one source.
fn load(
    path: &Path,
    written: &str,
    options: &DecodeOptions,
    sample_rate: u32,
    channels: usize,
    measure: bool,
) -> Result<Loaded, DspError> {
    let decoded = decode_file_with(path, options)?;
    if decoded.buffer.num_channels() == 0 || decoded.buffer.sample_rate == 0 {
        return Err(DspError::Decode {
            what: written.to_string(),
            message: "the file contains no audio".to_string(),
        });
    }
    // Decoder messages may name the absolute path; reports only ever show the path as written.
    let absolute = path.display().to_string();
    let scrub = |messages: &[String]| -> Vec<String> {
        messages
            .iter()
            .map(|w| w.replace(&absolute, written))
            .collect()
    };
    let warnings = scrub(&decoded.info.warnings);
    let notes = scrub(&decoded.info.notes);
    let resampled = resample::resample(&decoded.buffer, sample_rate)?;
    let buffer = adapt(resampled, channels);
    let lufs = if measure {
        loudness::integrated_lufs(&buffer)?
    } else {
        None
    };
    Ok(Loaded {
        source: Source { buffer, lufs },
        warnings,
        repaired: decoded.info.repaired_samples,
        downmix: decoded.info.downmix,
        notes,
    })
}

/// Channel layout of a source in a mix with `channels` output channels: mono output mixes everything down
/// (`(L + R)/2`, after the decoder's BS.775 downmix of wider sources); stereo output keeps mono sources
/// mono (they are panned at mix time). The decoder never returns more than two channels; a wider buffer
/// would still get the BS.775 downmix here.
fn adapt(buffer: AudioBuffer, channels: usize) -> AudioBuffer {
    let buffer = match soniccraft_dsp::downmix::to_stereo(&buffer) {
        Some((stereo, _)) => stereo,
        None => buffer,
    };
    match (channels, buffer.num_channels()) {
        (1, n) if n > 1 => buffer.to_mono(),
        _ => buffer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_adaptation() {
        let stereo = AudioBuffer::new(48_000, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert_eq!(adapt(stereo.clone(), 1).channels, vec![vec![0.5, 0.5]]);
        assert_eq!(adapt(stereo.clone(), 2), stereo);
        let mono = AudioBuffer::mono(48_000, vec![0.25]);
        assert_eq!(adapt(mono.clone(), 2), mono);
        // 3 channels in WAVE order: FL FR FC; the centre goes into both sides at -3 dB.
        let wide = AudioBuffer::new(48_000, vec![vec![1.0], vec![2.0], vec![3.0]]);
        let stereo = adapt(wide, 2);
        assert_eq!(stereo.num_channels(), 2);
        let c = 3.0 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((stereo.channels[0][0] - (1.0 + c)).abs() < 1e-6);
        assert!((stereo.channels[1][0] - (2.0 + c)).abs() < 1e-6);
    }

    #[test]
    fn repair_and_downmix_notes() {
        let d = repaired("assets/nan.wav", 3, "tracks[1].source", &["bed".into()]);
        assert_eq!(d.code, codes::AUDIO_REPAIRED);
        assert_eq!(
            d.message,
            "`assets/nan.wav` contained 3 non-finite samples (NaN/±∞); they were replaced with silence"
        );
        assert!(d.hint.as_deref().unwrap().contains("ffmpeg -i nan.wav"));
        let downmix = Downmix {
            source_channels: 6,
            layout: "FL FR FC LFE BL BR".into(),
            layout_from_file: true,
            dropped: vec!["LFE".into()],
        };
        let d = downmixed("center51.wav", &downmix, 2, "tracks[0].source", &[]);
        assert_eq!(d.severity, soniccraft_core::Severity::Info);
        assert_eq!(
            d.message,
            "`center51.wav` has 6 channels (FL FR FC LFE BL BR); it was mixed down to stereo with ITU-R BS.775 \
             (LFE dropped)"
        );
    }
}
