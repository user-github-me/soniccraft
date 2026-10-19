//! Waveform overview for the web preview (DESIGN.md §10.2): min/max of the mono mixdown per bucket.

use crate::AudioBuffer;

/// Min/max envelope of a buffer at a fixed number of points per second.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Waveform {
    /// Buckets per second of audio.
    pub points_per_second: u32,
    /// Minimum sample of each bucket, rounded to 3 decimals.
    pub min: Vec<f64>,
    /// Maximum sample of each bucket, rounded to 3 decimals.
    pub max: Vec<f64>,
}

/// Round to 3 decimals, normalizing −0.0 to 0.0.
fn round3(v: f32) -> f64 {
    let r = (f64::from(v) * 1000.0).round() / 1000.0;
    if r == 0.0 { 0.0 } else { r }
}

/// Compute the waveform of the mono mixdown (average of all channels) of `buffer`.
///
/// Bucket `k` covers frames `[⌊k·sr/pps⌋, ⌊(k+1)·sr/pps⌋)`; there are `⌈frames·pps/sr⌉` buckets. A
/// bucket that contains no frame (only possible when `pps > sr`) repeats the sample at its start.
/// Returns an empty waveform for `points_per_second == 0`, a zero sample rate or an empty buffer.
pub fn waveform(buffer: &AudioBuffer, points_per_second: u32) -> Waveform {
    let frames = buffer.frames();
    let sr = u128::from(buffer.sample_rate);
    let pps = u128::from(points_per_second);
    let mut out = Waveform {
        points_per_second,
        ..Waveform::default()
    };
    if pps == 0 || sr == 0 || frames == 0 || buffer.num_channels() == 0 {
        return out;
    }
    let mono = buffer.to_mono();
    let x = &mono.channels[0];
    let buckets = (frames as u128 * pps).div_ceil(sr) as usize;
    out.min.reserve(buckets);
    out.max.reserve(buckets);
    for k in 0..buckets {
        let start = ((k as u128 * sr / pps) as usize).min(frames - 1);
        let end = (((k as u128 + 1) * sr / pps) as usize).clamp(start + 1, frames);
        let (lo, hi) = x[start..end]
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        out.min.push(round3(lo));
        out.max.push(round3(hi));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_and_rounding() {
        let rate = 1000;
        // 2.5 s: a ramp from -1 to ~1 on L and silence on R → mono = ramp / 2.
        let n = 2500;
        let l: Vec<f32> = (0..n).map(|i| -1.0 + 2.0 * i as f32 / n as f32).collect();
        let buf = AudioBuffer::new(rate, vec![l.clone(), vec![0.0; n]]);
        let w = waveform(&buf, 10);
        assert_eq!(w.points_per_second, 10);
        assert_eq!(w.min.len(), 25);
        assert_eq!(w.max.len(), 25);
        assert_eq!(w.min[0], -0.5);
        assert_eq!(w.max[0], round3((l[99]) / 2.0));
        assert!(w.min.iter().zip(&w.max).all(|(a, b)| a <= b));
        for v in w.min.iter().chain(&w.max) {
            assert_eq!(*v, (v * 1000.0).round() / 1000.0, "3 decimals");
        }
    }

    #[test]
    fn partial_last_bucket_and_edge_cases() {
        let buf = AudioBuffer::mono(44_100, vec![0.25; 44_100 + 10]);
        let w = waveform(&buf, 50);
        assert_eq!(w.min.len(), 51);
        assert_eq!((w.min[50], w.max[50]), (0.25, 0.25));
        assert_eq!(waveform(&buf, 0).min.len(), 0);
        assert_eq!(
            waveform(&AudioBuffer::mono(8000, vec![]), 50),
            Waveform {
                points_per_second: 50,
                ..Default::default()
            }
        );
        // More points than samples: buckets repeat samples instead of being empty.
        let tiny = AudioBuffer::mono(10, vec![0.1, -0.2]);
        let w = waveform(&tiny, 20);
        assert_eq!(w.min.len(), 4);
        assert_eq!(w.max, vec![0.1, 0.1, -0.2, -0.2]);
        // -0.0 is normalized.
        let w = waveform(&AudioBuffer::mono(10, vec![-0.0001; 10]), 1);
        assert!(w.max[0].is_sign_positive());
    }
}
