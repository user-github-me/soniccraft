//! The length-determining part of the per-cue chain (DESIGN.md §4.1 steps 1–3), shared by renders and dry
//! runs so that a cached cue's planned length is exactly its rendered length.

use soniccraft_core::ir::{TrimConfig, ms_to_frames};
use soniccraft_dsp::{AudioBuffer, DspError, analysis, resample};

/// Level below which speech-region detection counts audio as a gap (DESIGN.md §5.4).
pub(crate) const REGION_THRESHOLD_DB: f64 = -45.0;
/// Shortest gap that splits two speech regions (DESIGN.md §5.4).
pub(crate) const REGION_MIN_GAP_MS: f64 = 120.0;

/// Provider audio → mono → output rate → silence-trimmed. Returns the trimmed audio and the number of
/// frames (at the output rate) cut from its start. A cue that is silent throughout is kept untrimmed.
pub(crate) fn prepare(
    audio: &AudioBuffer,
    sample_rate: u32,
    trim: Option<&TrimConfig>,
) -> Result<(AudioBuffer, usize), DspError> {
    let mono = audio.to_mono();
    let resampled = resample::resample(&mono, sample_rate)?;
    let Some(trim) = trim else {
        return Ok((resampled, 0));
    };
    let padding = frames(trim.padding_ms, sample_rate);
    match analysis::trim_bounds(&resampled, trim.threshold_db, padding) {
        Some(range) if range.start > 0 || range.end < resampled.frames() => {
            Ok((resampled.slice(range.start, range.end), range.start))
        }
        _ => Ok((resampled, 0)),
    }
}

/// Non-negative frame count of `ms` at `sample_rate`.
pub(crate) fn frames(ms: f64, sample_rate: u32) -> usize {
    usize::try_from(ms_to_frames(ms, sample_rate).max(0)).unwrap_or(0)
}

/// Milliseconds of `frames` at `sample_rate`.
pub(crate) fn ms(frames: usize, sample_rate: u32) -> f64 {
    if sample_rate == 0 {
        0.0
    } else {
        frames as f64 * 1000.0 / f64::from(sample_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_reports_the_offset() {
        let mut x = vec![0.0f32; 24_000];
        for (i, s) in x.iter_mut().enumerate().skip(6_000).take(12_000) {
            *s = 0.5 * (i as f32 * 0.05).sin();
        }
        let audio = AudioBuffer::new(24_000, vec![x.clone(), x]);
        let trim = TrimConfig {
            threshold_db: -50.0,
            padding_ms: 30.0,
        };
        let (out, start) = prepare(&audio, 48_000, Some(&trim)).unwrap();
        assert_eq!(out.num_channels(), 1);
        assert!(start > 0 && start < 12_000, "{start}");
        assert!(out.frames() < 48_000);
        let (untrimmed, zero) = prepare(&audio, 48_000, None).unwrap();
        assert_eq!((untrimmed.frames(), zero), (48_000, 0));
        let silent = AudioBuffer::silent(24_000, 1, 2_400);
        let (kept, offset) = prepare(&silent, 48_000, Some(&trim)).unwrap();
        assert_eq!((kept.frames(), offset), (4_800, 0));
    }
}
