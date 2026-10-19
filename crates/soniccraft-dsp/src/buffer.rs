//! Planar f32 audio buffers.

/// Planar 32-bit float audio: `channels[c][frame]`, nominal range [-1, 1].
///
/// Every channel must have the same length; functions that take buffers from callers validate this
/// and return [`crate::DspError::InvalidInput`] for ragged buffers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioBuffer {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// One `Vec` of samples per channel (canonical order: L, R, C, ...).
    pub channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    /// Wrap planar channel data. All channels must have the same length.
    pub fn new(sample_rate: u32, channels: Vec<Vec<f32>>) -> Self {
        debug_assert!(
            channels.windows(2).all(|w| w[0].len() == w[1].len()),
            "ragged channels"
        );
        AudioBuffer {
            sample_rate,
            channels,
        }
    }

    /// A mono buffer.
    pub fn mono(sample_rate: u32, samples: Vec<f32>) -> Self {
        AudioBuffer {
            sample_rate,
            channels: vec![samples],
        }
    }

    /// `frames` frames of silence with `num_channels` channels.
    pub fn silent(sample_rate: u32, num_channels: usize, frames: usize) -> Self {
        AudioBuffer {
            sample_rate,
            channels: vec![vec![0.0; frames]; num_channels],
        }
    }

    /// Deinterleave `samples` (`[L R L R …]`) into a buffer with `num_channels` channels.
    pub fn from_interleaved(sample_rate: u32, num_channels: usize, samples: &[f32]) -> Self {
        assert!(num_channels > 0, "num_channels must be > 0");
        let frames = samples.len() / num_channels;
        let mut channels = vec![Vec::with_capacity(frames); num_channels];
        for frame in samples.chunks_exact(num_channels) {
            for (c, &s) in frame.iter().enumerate() {
                channels[c].push(s);
            }
        }
        AudioBuffer {
            sample_rate,
            channels,
        }
    }

    /// Interleave into `[L R L R …]`.
    pub fn to_interleaved(&self) -> Vec<f32> {
        let n = self.num_channels();
        let frames = self.frames();
        let mut out = Vec::with_capacity(frames * n);
        for i in 0..frames {
            for ch in &self.channels {
                // A ragged buffer pads short channels with silence instead of panicking.
                out.push(ch.get(i).copied().unwrap_or(0.0));
            }
        }
        out
    }

    /// Number of channels.
    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    /// Number of frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    /// True when the buffer has no frames.
    pub fn is_empty(&self) -> bool {
        self.frames() == 0
    }

    /// Duration in seconds (0 for a zero sample rate).
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.frames() as f64 / self.sample_rate as f64
    }

    /// Average all channels into one.
    pub fn to_mono(&self) -> AudioBuffer {
        match self.num_channels() {
            0 => AudioBuffer::mono(self.sample_rate, Vec::new()),
            1 => self.clone(),
            n => {
                let frames = self.frames();
                let scale = 1.0 / n as f32;
                let mut out = vec![0.0f32; frames];
                for ch in &self.channels {
                    for (o, s) in out.iter_mut().zip(ch) {
                        *o += *s;
                    }
                }
                for o in &mut out {
                    *o *= scale;
                }
                AudioBuffer::mono(self.sample_rate, out)
            }
        }
    }

    /// Convert to `n` channels without panning: mono is duplicated, multichannel is averaged down to mono
    /// first when `n == 1`, and stereo-or-more → stereo keeps the first two channels.
    pub fn to_channels(&self, n: usize) -> AudioBuffer {
        assert!(n > 0, "channel count must be > 0");
        let have = self.num_channels();
        if have == n {
            return self.clone();
        }
        if n == 1 {
            return self.to_mono();
        }
        if have == 1 {
            return AudioBuffer::new(self.sample_rate, vec![self.channels[0].clone(); n]);
        }
        let mut channels: Vec<Vec<f32>> = self.channels.iter().take(n).cloned().collect();
        while channels.len() < n {
            channels.push(channels[channels.len() % have].clone());
        }
        AudioBuffer::new(self.sample_rate, channels)
    }

    /// Maximum absolute sample value over all channels.
    pub fn peak(&self) -> f32 {
        self.channels
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// Replace every non-finite sample (NaN, +∞, −∞) with 0 (silence) and return how many were
    /// replaced. Every decoder runs this on its output, so decoded audio is always finite.
    pub fn replace_non_finite(&mut self) -> u64 {
        let mut replaced = 0u64;
        for ch in &mut self.channels {
            for s in ch.iter_mut() {
                if !s.is_finite() {
                    *s = 0.0;
                    replaced += 1;
                }
            }
        }
        replaced
    }

    /// Multiply every sample by `gain`.
    pub fn apply_gain(&mut self, gain: f32) {
        if gain == 1.0 {
            return;
        }
        for ch in &mut self.channels {
            for s in ch.iter_mut() {
                *s *= gain;
            }
        }
    }

    /// Keep frames `[start, end)` (clamped).
    pub fn slice(&self, start: usize, end: usize) -> AudioBuffer {
        let frames = self.frames();
        let s = start.min(frames);
        let e = end.clamp(s, frames);
        AudioBuffer::new(
            self.sample_rate,
            self.channels
                .iter()
                .map(|c| c[s.min(c.len())..e.min(c.len())].to_vec())
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_roundtrip() {
        let buf = AudioBuffer::new(48_000, vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]]);
        let inter = buf.to_interleaved();
        assert_eq!(inter, vec![1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        assert_eq!(AudioBuffer::from_interleaved(48_000, 2, &inter), buf);
    }

    #[test]
    fn channel_conversion() {
        let mono = AudioBuffer::mono(24_000, vec![0.5, -0.5]);
        let stereo = mono.to_channels(2);
        assert_eq!(stereo.channels, vec![vec![0.5, -0.5], vec![0.5, -0.5]]);
        assert_eq!(stereo.to_mono().channels, vec![vec![0.5, -0.5]]);
        assert_eq!(stereo.frames(), 2);
        assert!((stereo.duration_secs() - 2.0 / 24_000.0).abs() < 1e-12);
    }

    #[test]
    fn wide_layouts_and_helpers() {
        let three = AudioBuffer::new(8000, vec![vec![0.3, 0.6], vec![0.0, 0.0], vec![-0.3, 0.3]]);
        assert_eq!(
            three.to_channels(2).channels,
            vec![vec![0.3, 0.6], vec![0.0, 0.0]]
        );
        let mono = three.to_mono();
        assert!((mono.channels[0][1] - 0.3).abs() < 1e-7);
        let stereo = AudioBuffer::new(8000, vec![vec![1.0], vec![-2.0]]);
        assert_eq!(
            stereo.to_channels(3).channels,
            vec![vec![1.0], vec![-2.0], vec![1.0]]
        );
        assert_eq!(stereo.peak(), 2.0);
        let mut g = stereo.clone();
        g.apply_gain(0.5);
        assert_eq!(g.channels, vec![vec![0.5], vec![-1.0]]);
        assert_eq!(AudioBuffer::default().duration_secs(), 0.0);
        assert!(AudioBuffer::silent(8000, 2, 0).is_empty());
    }

    #[test]
    fn non_finite_samples_become_silence() {
        let mut b = AudioBuffer::new(
            8000,
            vec![
                vec![0.5, f32::NAN, f32::INFINITY],
                vec![f32::NEG_INFINITY, -0.25, 1e30],
            ],
        );
        assert_eq!(b.replace_non_finite(), 3);
        assert_eq!(
            b.channels,
            vec![vec![0.5, 0.0, 0.0], vec![0.0, -0.25, 1e30]]
        );
        assert_eq!(b.replace_non_finite(), 0, "idempotent");
    }

    #[test]
    fn slice_clamps() {
        let b = AudioBuffer::mono(8000, vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(b.slice(1, 3).channels, vec![vec![1.0, 2.0]]);
        assert_eq!(b.slice(3, 100).channels, vec![vec![3.0]]);
        assert_eq!(b.slice(10, 2).channels, vec![Vec::<f32>::new()]);
        assert_eq!(b.slice(2, 1).frames(), 0);
    }
}
