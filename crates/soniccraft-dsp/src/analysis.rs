//! Level analysis: dB conversions, RMS/peak helpers, silence trimming bounds (DESIGN.md §4.1 step 3)
//! and speech-region detection for word-timing estimation (§5.4).
//!
//! Silence analysis works on consecutive 5 ms frames. A frame's level is the RMS of its samples in
//! dBFS (a full-scale sine reads −3 dBFS); for multichannel audio the loudest channel counts, so a
//! frame is silent only when every channel is.

use std::ops::Range;

use crate::AudioBuffer;

/// Length of the RMS analysis frames used by [`trim_bounds`] and [`speech_regions`].
pub const ANALYSIS_FRAME_MS: f64 = 5.0;

/// `20·log10(x)`; `-inf` for 0, NaN for negative or NaN input.
pub fn linear_to_db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// `10^(db/20)`.
pub fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// RMS over all samples of all channels (0 for an empty buffer).
pub fn rms(buffer: &AudioBuffer) -> f64 {
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for ch in &buffer.channels {
        sum += ch.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>();
        count += ch.len();
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

/// [`rms`] in dBFS (`-inf` for silence).
pub fn rms_dbfs(buffer: &AudioBuffer) -> f64 {
    linear_to_db(rms(buffer))
}

/// Maximum absolute sample value over all channels (0 for an empty buffer).
pub fn peak(buffer: &AudioBuffer) -> f64 {
    f64::from(buffer.peak())
}

/// [`peak`] in dBFS (`-inf` for silence).
pub fn peak_dbfs(buffer: &AudioBuffer) -> f64 {
    linear_to_db(peak(buffer))
}

/// Number of samples in one analysis frame at `sample_rate` (at least 1).
pub fn analysis_frame_len(sample_rate: u32) -> usize {
    ((ANALYSIS_FRAME_MS * f64::from(sample_rate) / 1000.0).round() as usize).max(1)
}

/// Per-frame loudness classification: `true` when the frame's RMS (loudest channel) is above
/// `threshold_db` dBFS. The last frame may be shorter than [`analysis_frame_len`].
fn loud_frames(buffer: &AudioBuffer, threshold_db: f64) -> (usize, Vec<bool>) {
    let len = analysis_frame_len(buffer.sample_rate);
    let frames = buffer.frames();
    // Compare mean squares against the squared threshold: no log per frame.
    let threshold_ms = db_to_linear(threshold_db).powi(2);
    let count = frames.div_ceil(len);
    let mut loud = vec![false; count];
    for (k, flag) in loud.iter_mut().enumerate() {
        let range = k * len..((k + 1) * len).min(frames);
        let n = range.len() as f64;
        *flag = buffer.channels.iter().any(|ch| {
            // Clamp to the channel (a ragged buffer must not panic).
            let window = &ch[range.start.min(ch.len())..range.end.min(ch.len())];
            let sum: f64 = window.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            sum / n > threshold_ms
        });
    }
    (len, loud)
}

/// Silence-trim bounds (DESIGN.md §4.1 step 3): the frame range from the first to the last 5 ms
/// analysis frame whose RMS is above `threshold_db` dBFS, widened by `padding_frames` on each side and
/// clamped to the buffer. `None` when every frame is at or below the threshold (or the buffer is empty).
pub fn trim_bounds(
    buffer: &AudioBuffer,
    threshold_db: f64,
    padding_frames: usize,
) -> Option<Range<usize>> {
    let frames = buffer.frames();
    let (len, loud) = loud_frames(buffer, threshold_db);
    let first = loud.iter().position(|&l| l)?;
    let last = loud.iter().rposition(|&l| l)?;
    let start = (first * len).saturating_sub(padding_frames);
    let end = ((last + 1) * len)
        .min(frames)
        .saturating_add(padding_frames)
        .min(frames);
    Some(start..end)
}

/// Non-silent regions (DESIGN.md §5.4): maximal frame ranges of audio above `threshold_db` dBFS, where
/// silent stretches shorter than `min_gap_frames` do not split a region. Regions start at the first and
/// end after the last loud 5 ms analysis frame they contain, so leading and trailing silence is excluded.
/// Sorted and non-overlapping; empty for silence.
pub fn speech_regions(
    buffer: &AudioBuffer,
    threshold_db: f64,
    min_gap_frames: usize,
) -> Vec<Range<usize>> {
    let frames = buffer.frames();
    let (len, loud) = loud_frames(buffer, threshold_db);
    let mut regions: Vec<Range<usize>> = Vec::new();
    let mut k = 0;
    while k < loud.len() {
        if !loud[k] {
            k += 1;
            continue;
        }
        let run_start = k;
        while k < loud.len() && loud[k] {
            k += 1;
        }
        let start = run_start * len;
        let end = (k * len).min(frames);
        match regions.last_mut() {
            // Bridge gaps shorter than the minimum.
            Some(prev) if start - prev.end < min_gap_frames => prev.end = end,
            _ => regions.push(start..end),
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, secs: f64, amp: f32) -> Vec<f32> {
        let n = (secs * f64::from(rate)).round() as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn db_helpers() {
        assert_eq!(linear_to_db(1.0), 0.0);
        assert!((linear_to_db(0.5) + 6.0206).abs() < 1e-4);
        assert_eq!(linear_to_db(0.0), f64::NEG_INFINITY);
        assert!((db_to_linear(-20.0) - 0.1).abs() < 1e-12);
        let sine = AudioBuffer::mono(48_000, tone(48_000, 1.0, 1.0));
        assert!(
            (rms_dbfs(&sine) + 3.0103).abs() < 0.01,
            "full-scale sine RMS is -3 dBFS"
        );
        assert!((peak(&sine) - 1.0).abs() < 1e-3);
        assert_eq!(rms(&AudioBuffer::default()), 0.0);
        assert_eq!(
            peak_dbfs(&AudioBuffer::silent(8000, 1, 10)),
            f64::NEG_INFINITY
        );
        assert_eq!(analysis_frame_len(48_000), 240);
        assert_eq!(analysis_frame_len(44_100), 221);
        assert_eq!(analysis_frame_len(100), 1);
    }

    #[test]
    fn trim_bounds_with_padding() {
        let rate = 48_000;
        // 100 ms silence, 500 ms tone, 200 ms silence.
        let mut x = vec![0.0f32; 4800];
        x.extend(tone(rate, 0.5, 0.3));
        x.extend(vec![0.0f32; 9600]);
        let buf = AudioBuffer::mono(rate, x);
        let pad = 1440; // 30 ms
        let r = trim_bounds(&buf, -50.0, pad).unwrap();
        // Tone starts exactly on an analysis frame boundary (4800 = 20 × 240).
        assert_eq!(r.start, 4800 - pad);
        assert_eq!(r.end, 4800 + 24_000 + pad);
        // No padding, and padding larger than the margins (clamped).
        assert_eq!(trim_bounds(&buf, -50.0, 0).unwrap(), 4800..28_800);
        assert_eq!(
            trim_bounds(&buf, -50.0, 1_000_000).unwrap(),
            0..buf.frames()
        );
    }

    #[test]
    fn trim_threshold_and_silence() {
        let rate = 16_000;
        let quiet = AudioBuffer::mono(rate, tone(rate, 0.2, 0.001)); // ≈ -63 dBFS RMS
        assert_eq!(trim_bounds(&quiet, -50.0, 0), None);
        assert!(trim_bounds(&quiet, -70.0, 0).is_some());
        assert_eq!(
            trim_bounds(&AudioBuffer::silent(rate, 2, 1000), -50.0, 10),
            None
        );
        assert_eq!(
            trim_bounds(&AudioBuffer::mono(rate, vec![]), -50.0, 10),
            None
        );
        // Stereo: content in one channel is enough.
        let mut l = vec![0.0f32; 1600];
        l.extend(tone(rate, 0.1, 0.5));
        let r = vec![0.0f32; l.len()];
        let st = AudioBuffer::new(rate, vec![r, l]);
        assert_eq!(trim_bounds(&st, -50.0, 0).unwrap(), 1600..3200);
    }

    #[test]
    fn trim_partial_last_frame() {
        let rate = 48_000;
        let mut x = vec![0.0f32; 1000];
        x.extend(vec![0.5f32; 10]); // loud samples in the last, partial frame (1000..1010)
        let buf = AudioBuffer::mono(rate, x);
        assert_eq!(trim_bounds(&buf, -50.0, 0).unwrap(), 960..1010);
    }

    #[test]
    fn speech_regions_split_on_long_gaps_only() {
        let rate = 48_000;
        let word = tone(rate, 0.3, 0.3); // 14400 frames = 60 analysis frames
        let short_gap = vec![0.0f32; 2400]; // 50 ms
        let long_gap = vec![0.0f32; 7200]; // 150 ms
        let mut x = vec![0.0f32; 4800];
        x.extend(&word);
        x.extend(&short_gap);
        x.extend(&word);
        x.extend(&long_gap);
        x.extend(&word);
        x.extend(vec![0.0f32; 4800]);
        let buf = AudioBuffer::mono(rate, x);
        let min_gap = 5760; // 120 ms
        let regions = speech_regions(&buf, -45.0, min_gap);
        let w = word.len();
        assert_eq!(
            regions,
            vec![
                4800..4800 + 2 * w + 2400,
                4800 + 2 * w + 2400 + 7200..4800 + 3 * w + 2400 + 7200
            ]
        );
        // A gap exactly min_gap long splits.
        let regions = speech_regions(&buf, -45.0, 7200);
        assert_eq!(regions.len(), 2);
        // min_gap 0: every silent stretch splits.
        assert_eq!(speech_regions(&buf, -45.0, 0).len(), 3);
        assert!(speech_regions(&AudioBuffer::silent(rate, 1, 48_000), -45.0, min_gap).is_empty());
    }
}
