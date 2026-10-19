//! Placing audio on the timeline: clip rendering (DESIGN.md §4.3), pan (§4.4), mixing, and the master
//! de-click (§4.6.2).
//!
//! # Fade conventions
//!
//! Fade gains are evaluated at sample *centres*: frame `i` of an `n`-frame fade-in gets
//! `curve((i + 0.5) / n)`, and frame `i` of an `n`-frame fade-out ending at frame `L` (exclusive) gets
//! `curve((L − i − 0.5) / n)`. A fade-out is therefore the exact time-reverse of a fade-in, and two
//! overlapping complementary fades (crossfades, loop seams) sum to exactly constant power (equal power)
//! or constant amplitude (linear).

use std::f64::consts::FRAC_PI_2;

use crate::{AudioBuffer, DspError};

/// Fade curve shapes (DESIGN.md §4.3). The gain for `t ∈ [0, 1]` is the fade-in gain; fade-outs use
/// `1 − t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FadeCurve {
    /// `t`.
    Linear,
    /// `sin(t·π/2)` (the default).
    #[default]
    EqualPower,
    /// `(10^(t·60/20) − 1) / (10^3 − 1)`: a 60 dB exponential ramp.
    Exponential,
}

impl FadeCurve {
    /// Fade-in gain at `t` (clamped to `[0, 1]`).
    pub fn gain(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear => t,
            FadeCurve::EqualPower => (t * FRAC_PI_2).sin(),
            FadeCurve::Exponential => (10f64.powf(t * 3.0) - 1.0) / 999.0,
        }
    }
}

/// How one clip plays on the timeline (all lengths in frames at the timeline rate).
#[derive(Debug, Clone, PartialEq)]
pub struct ClipSpec {
    /// Timeline frame of the clip's first frame. May be negative: frames before 0 are discarded.
    pub start: i64,
    /// Number of frames the clip plays.
    pub length: usize,
    /// First source frame to play (`trim_start`).
    pub source_offset: usize,
    /// Loop the source (from `source_offset`) to fill `length`. Without looping, frames past the end of
    /// the source are silent.
    pub looped: bool,
    /// Equal-power crossfade at each loop seam (clamped to half the loop segment).
    pub loop_crossfade: usize,
    /// Fade-in length, relative to the clip start.
    pub fade_in: usize,
    /// Fade-out length, relative to the clip end.
    pub fade_out: usize,
    /// Curve of `fade_in`/`fade_out`.
    pub fade_curve: FadeCurve,
    /// Equal-power fade-in from a crossfade with the previous clip (multiplies with `fade_in`).
    pub crossfade_in: usize,
    /// Equal-power fade-out into a crossfade with the next clip (multiplies with `fade_out`).
    pub crossfade_out: usize,
    /// Linear gain.
    pub gain: f32,
}

impl ClipSpec {
    /// A clip of `length` frames at `start` playing the source from frame 0 at unity gain, no fades.
    pub fn new(start: i64, length: usize) -> Self {
        ClipSpec {
            start,
            length,
            source_offset: 0,
            looped: false,
            loop_crossfade: 0,
            fade_in: 0,
            fade_out: 0,
            fade_curve: FadeCurve::EqualPower,
            crossfade_in: 0,
            crossfade_out: 0,
            gain: 1.0,
        }
    }

    /// `(fade_in, fade_out)` after proportional scaling when they do not fit in the clip
    /// (DESIGN.md §3.3; layout normally does this already).
    pub fn effective_fades(&self) -> (usize, usize) {
        scale_pair(self.fade_in, self.fade_out, self.length)
    }
}

/// Scale `(a, b)` proportionally so that `a + b <= total`.
fn scale_pair(a: usize, b: usize, total: usize) -> (usize, usize) {
    let sum = a as u128 + b as u128;
    if sum <= total as u128 {
        return (a, b);
    }
    let a2 = ((a as u128 * total as u128 + sum / 2) / sum) as usize;
    (a2.min(total), total - a2.min(total))
}

/// Gain envelope of a clip at clip frame `i` (fades and crossfades; not the clip gain).
struct Envelope {
    length: usize,
    fade_in: usize,
    fade_out: usize,
    curve: FadeCurve,
    xfade_in: usize,
    xfade_out: usize,
}

impl Envelope {
    fn of(spec: &ClipSpec) -> Self {
        let (fade_in, fade_out) = spec.effective_fades();
        let (xfade_in, xfade_out) = scale_pair(spec.crossfade_in, spec.crossfade_out, spec.length);
        Envelope {
            length: spec.length,
            fade_in,
            fade_out,
            curve: spec.fade_curve,
            xfade_in,
            xfade_out,
        }
    }

    fn at(&self, i: usize) -> f64 {
        let mut g = 1.0;
        let from_end = (self.length - i) as f64 - 0.5;
        if i < self.fade_in {
            g *= self.curve.gain((i as f64 + 0.5) / self.fade_in as f64);
        }
        if self.length - i <= self.fade_out {
            g *= self.curve.gain(from_end / self.fade_out as f64);
        }
        if i < self.xfade_in {
            g *= FadeCurve::EqualPower.gain((i as f64 + 0.5) / self.xfade_in as f64);
        }
        if self.length - i <= self.xfade_out {
            g *= FadeCurve::EqualPower.gain(from_end / self.xfade_out as f64);
        }
        g
    }
}

/// Reads source frames for clip positions, handling the loop seams.
struct SourceReader<'a> {
    source: &'a AudioBuffer,
    offset: usize,
    segment: usize,
    looped: bool,
    xfade: usize,
}

impl<'a> SourceReader<'a> {
    fn new(source: &'a AudioBuffer, spec: &ClipSpec) -> Self {
        let offset = spec.source_offset.min(source.frames());
        let segment = source.frames() - offset;
        let xfade = if spec.looped {
            spec.loop_crossfade.min(segment / 2)
        } else {
            0
        };
        SourceReader {
            source,
            offset,
            segment,
            looped: spec.looped,
            xfade,
        }
    }

    /// Sample of channel `ch` at clip frame `i`.
    fn sample(&self, ch: usize, i: usize) -> f32 {
        let data = &self.source.channels[ch];
        // Missing samples (a ragged buffer) read as silence instead of panicking.
        let at = |k: usize| data.get(self.offset + k).copied().unwrap_or(0.0);
        if self.segment == 0 {
            return 0.0;
        }
        if !self.looped {
            return if i < self.segment { at(i) } else { 0.0 };
        }
        let period = self.segment - self.xfade;
        let (k, r) = (i / period, i % period);
        if k == 0 || r >= self.xfade {
            return at(r);
        }
        // Seam k: the next pass fades in from the segment start while the previous pass fades out
        // over the segment tail.
        let t = (r as f64 + 0.5) / self.xfade as f64;
        let fade_in = (t * FRAC_PI_2).sin() as f32;
        let fade_out = (t * FRAC_PI_2).cos() as f32;
        at(r) * fade_in + at(period + r) * fade_out
    }
}

/// Render a whole clip (`spec.length` frames, the source's channel count and sample rate) with
/// source offset, looping, fades and gain applied. `spec.start` is ignored.
pub fn render_clip(source: &AudioBuffer, spec: &ClipSpec) -> AudioBuffer {
    render_clip_range(source, spec, 0..spec.length)
}

/// Render clip frames `range` (clamped to the clip) — the building block of [`render_clip`] and
/// [`mix_clip`].
fn render_clip_range(
    source: &AudioBuffer,
    spec: &ClipSpec,
    range: std::ops::Range<usize>,
) -> AudioBuffer {
    let range = range.start.min(spec.length)..range.end.min(spec.length);
    let reader = SourceReader::new(source, spec);
    let envelope = Envelope::of(spec);
    let gains: Vec<f32> = range
        .clone()
        .map(|i| (envelope.at(i) * f64::from(spec.gain)) as f32)
        .collect();
    let channels = (0..source.num_channels())
        .map(|ch| {
            range
                .clone()
                .zip(&gains)
                .map(|(i, &g)| reader.sample(ch, i) * g)
                .collect()
        })
        .collect();
    AudioBuffer::new(source.sample_rate, channels)
}

/// Render `spec` and mix the part that falls inside `dest` (timeline frames `[0, dest.frames())`)
/// into `dest`, panned with [`mix_into`]'s channel rules.
///
/// Errors when the source and destination sample rates differ or `dest` has ragged channels.
pub fn mix_clip(
    dest: &mut AudioBuffer,
    source: &AudioBuffer,
    spec: &ClipSpec,
    pan: f32,
) -> Result<(), DspError> {
    check_rates(dest, source)?;
    check_layout(dest)?;
    let total = dest.frames() as i64;
    let first = spec.start.max(0);
    let last = spec.start.saturating_add(spec.length as i64).min(total);
    if first >= last || source.num_channels() == 0 {
        return Ok(());
    }
    let range = (first - spec.start) as usize..(last - spec.start) as usize;
    let rendered = render_clip_range(source, spec, range);
    mix_into(dest, &rendered, first, 1.0, pan)
}

/// Per-channel gains of the constant-power pan law for a mono source (DESIGN.md §4.4):
/// `θ = (pan + 1)·π/4`, `(L, R) = (cos θ, sin θ)`. `pan` is clamped to `[-1, 1]`; centre is −3 dB per
/// side, which keeps the BS.1770 loudness of the mono source.
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let theta = (f64::from(pan.clamp(-1.0, 1.0)) + 1.0) * std::f64::consts::FRAC_PI_4;
    (theta.cos() as f32, theta.sin() as f32)
}

/// Per-channel gains of the stereo balance law (DESIGN.md §4.4): `pan < 0` scales R by `1 + pan`,
/// `pan > 0` scales L by `1 − pan`. `pan` is clamped to `[-1, 1]`.
pub fn balance_gains(pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    if pan < 0.0 {
        (1.0, 1.0 + pan)
    } else {
        (1.0 - pan, 1.0)
    }
}

/// Errors on ragged channels.
fn check_layout(buffer: &AudioBuffer) -> Result<(), DspError> {
    let frames = buffer.frames();
    if buffer.channels.iter().any(|c| c.len() != frames) {
        return Err(DspError::InvalidInput(
            "channels have different lengths".into(),
        ));
    }
    Ok(())
}

fn check_rates(dest: &AudioBuffer, src: &AudioBuffer) -> Result<(), DspError> {
    if dest.sample_rate != src.sample_rate {
        return Err(DspError::InvalidInput(format!(
            "cannot mix {} Hz audio into a {} Hz buffer; resample first",
            src.sample_rate, dest.sample_rate
        )));
    }
    Ok(())
}

/// Add `src × gain` into `dest` starting at timeline frame `at` (may be negative); frames outside
/// `dest` are discarded. Channels are mapped with the DESIGN.md §4.4 rules:
///
/// * mono `dest`: `src` is averaged to mono (`(L + R)/2` for stereo), `pan` ignored;
/// * stereo `dest`: a mono `src` is panned with [`pan_gains`]; a stereo (or wider: the first two
///   channels) `src` gets [`balance_gains`];
/// * any other `dest` layout: channel `c` goes to channel `c` (extra channels dropped), `pan` ignored.
///
/// Errors when the sample rates differ or either buffer has ragged channels.
pub fn mix_into(
    dest: &mut AudioBuffer,
    src: &AudioBuffer,
    at: i64,
    gain: f32,
    pan: f32,
) -> Result<(), DspError> {
    check_rates(dest, src)?;
    check_layout(dest)?;
    check_layout(src)?;
    let total = dest.frames() as i64;
    let first = at.max(0);
    let last = at.saturating_add(src.frames() as i64).min(total);
    if first >= last || src.num_channels() == 0 || dest.num_channels() == 0 {
        return Ok(());
    }
    let (d0, s0, n) = (
        first as usize,
        (first - at) as usize,
        (last - first) as usize,
    );
    let src_ch = src.num_channels();
    let add = |dst: &mut [f32], s: &[f32], g: f32| {
        if g != 0.0 {
            for (d, &x) in dst.iter_mut().zip(s) {
                *d += x * g;
            }
        }
    };
    match dest.num_channels() {
        1 => {
            let g = gain / src_ch as f32;
            let dst = &mut dest.channels[0][d0..d0 + n];
            for ch in &src.channels {
                add(dst, &ch[s0..s0 + n], g);
            }
        }
        2 => {
            let (gl, gr) = if src_ch == 1 {
                pan_gains(pan)
            } else {
                balance_gains(pan)
            };
            let (l_src, r_src) = if src_ch == 1 { (0, 0) } else { (0, 1) };
            let (left, right) = dest.channels.split_at_mut(1);
            add(
                &mut left[0][d0..d0 + n],
                &src.channels[l_src][s0..s0 + n],
                gain * gl,
            );
            add(
                &mut right[0][d0..d0 + n],
                &src.channels[r_src][s0..s0 + n],
                gain * gr,
            );
        }
        _ => {
            for (dst, s) in dest.channels.iter_mut().zip(&src.channels) {
                add(&mut dst[d0..d0 + n], &s[s0..s0 + n], gain);
            }
        }
    }
    Ok(())
}

/// Length of the master de-click fade (DESIGN.md §4.6.2).
pub const DECLICK_MS: f64 = 10.0;

/// Peak below which the end of the master counts as silent for the de-click: half a 16-bit LSB
/// (≈ −96 dBFS), i.e. anything that would quantize to digital silence.
pub const DECLICK_SILENCE_PEAK: f32 = 1.0 / 65_536.0;

/// Master de-click (DESIGN.md §4.6.2): if the last 10 ms are not silent, apply a 10 ms linear
/// fade-out that reaches exactly 0 on the final frame. Returns whether the fade was applied.
pub fn declick_tail(buffer: &mut AudioBuffer) -> bool {
    let frames = buffer.frames();
    let n = ((DECLICK_MS * f64::from(buffer.sample_rate) / 1000.0).round() as usize).min(frames);
    if n == 0 {
        return false;
    }
    let tail_peak = buffer
        .channels
        .iter()
        .flat_map(|c| c[frames - n..].iter())
        .fold(0.0f32, |m, s| m.max(s.abs()));
    if tail_peak <= DECLICK_SILENCE_PEAK {
        return false;
    }
    for ch in &mut buffer.channels {
        for (j, s) in ch[frames - n..].iter_mut().rev().enumerate() {
            // j = 0 is the last frame.
            *s *= j as f32 / n as f32;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_source(rate: u32, n: usize) -> AudioBuffer {
        AudioBuffer::mono(rate, (0..n).map(|i| i as f32).collect())
    }

    #[test]
    fn fade_curve_formulas() {
        for t in [0.0, 0.1, 0.25, 0.5, 0.9, 1.0] {
            assert_eq!(FadeCurve::Linear.gain(t), t);
            assert!(
                (FadeCurve::EqualPower.gain(t) - (t * std::f64::consts::PI / 2.0).sin()).abs()
                    < 1e-15
            );
            let exp = (10f64.powf(t * 60.0 / 20.0) - 1.0) / (10f64.powi(3) - 1.0);
            assert!((FadeCurve::Exponential.gain(t) - exp).abs() < 1e-15);
        }
        assert_eq!(FadeCurve::Exponential.gain(0.0), 0.0);
        assert!((FadeCurve::Exponential.gain(1.0) - 1.0).abs() < 1e-12);
        // -60 dB relative span: at t = 2/3 the exponential fade is ~ -20 dB.
        assert!((20.0 * FadeCurve::Exponential.gain(2.0 / 3.0).log10() + 20.04).abs() < 0.1);
        assert_eq!(FadeCurve::Linear.gain(-1.0), 0.0);
        assert_eq!(FadeCurve::Linear.gain(2.0), 1.0);
    }

    #[test]
    fn fades_are_applied_at_sample_centres() {
        let src = AudioBuffer::mono(1000, vec![1.0; 100]);
        for curve in [
            FadeCurve::Linear,
            FadeCurve::EqualPower,
            FadeCurve::Exponential,
        ] {
            let spec = ClipSpec {
                fade_in: 10,
                fade_out: 20,
                fade_curve: curve,
                gain: 0.5,
                ..ClipSpec::new(0, 100)
            };
            let out = render_clip(&src, &spec);
            let x = &out.channels[0];
            for (i, &v) in x.iter().enumerate() {
                let want = match i {
                    0..10 => 0.5 * curve.gain((i as f64 + 0.5) / 10.0),
                    10..80 => 0.5,
                    _ => 0.5 * curve.gain((100.0 - i as f64 - 0.5) / 20.0),
                };
                assert!(
                    (f64::from(v) - want).abs() < 1e-6,
                    "{curve:?} frame {i}: {v} vs {want}"
                );
            }
            // Fade-out is the time reverse of a fade-in of the same length.
            let spec_in = ClipSpec {
                fade_in: 20,
                fade_curve: curve,
                gain: 0.5,
                ..ClipSpec::new(0, 100)
            };
            let fin = render_clip(&src, &spec_in);
            for k in 0..20 {
                assert!((fin.channels[0][k] - x[99 - k]).abs() < 1e-7);
            }
        }
    }

    #[test]
    fn fades_scale_when_too_long() {
        let spec = ClipSpec {
            fade_in: 300,
            fade_out: 100,
            ..ClipSpec::new(0, 200)
        };
        assert_eq!(spec.effective_fades(), (150, 50));
        let spec = ClipSpec {
            fade_in: 10,
            fade_out: 10,
            ..ClipSpec::new(0, 200)
        };
        assert_eq!(spec.effective_fades(), (10, 10));
    }

    #[test]
    fn source_offset_and_natural_end() {
        let src = ramp_source(1000, 10);
        let spec = ClipSpec {
            source_offset: 4,
            ..ClipSpec::new(0, 8)
        };
        assert_eq!(
            render_clip(&src, &spec).channels[0],
            vec![4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 0.0, 0.0]
        );
        let spec = ClipSpec {
            source_offset: 50,
            ..ClipSpec::new(0, 3)
        };
        assert_eq!(render_clip(&src, &spec).channels[0], vec![0.0; 3]);
    }

    #[test]
    fn looping_wraps_to_the_offset() {
        let src = ramp_source(1000, 6);
        let spec = ClipSpec {
            source_offset: 2,
            looped: true,
            ..ClipSpec::new(0, 10)
        };
        assert_eq!(
            render_clip(&src, &spec).channels[0],
            vec![2.0, 3.0, 4.0, 5.0, 2.0, 3.0, 4.0, 5.0, 2.0, 3.0]
        );
    }

    #[test]
    fn loop_crossfade_is_equal_power_and_continuous() {
        // A constant-amplitude source: an equal-power seam crossfade of two identical (correlated)
        // signals sums to sin + cos, between 1 and √2, and returns to exactly 1 outside the seam.
        let n = 1000;
        let src = AudioBuffer::mono(1000, vec![1.0; n]);
        let x = 100;
        let spec = ClipSpec {
            looped: true,
            loop_crossfade: x,
            ..ClipSpec::new(0, 3000)
        };
        let out = render_clip(&src, &spec);
        let y = &out.channels[0];
        let period = n - x;
        for (i, &v) in y.iter().enumerate() {
            let (k, r) = (i / period, i % period);
            if k >= 1 && r < x {
                let t = (r as f64 + 0.5) / x as f64;
                let want = (t * FRAC_PI_2).sin() + (t * FRAC_PI_2).cos();
                assert!((f64::from(v) - want).abs() < 1e-5);
            } else {
                assert_eq!(v, 1.0);
            }
        }
        // Uncorrelated content keeps its power: the gains are cos/sin.
        let t = 0.3f64;
        assert!(
            (((t * FRAC_PI_2).sin()).powi(2) + ((t * FRAC_PI_2).cos()).powi(2) - 1.0).abs() < 1e-12
        );
        // A ramp source: the seam blends the tail into the head.
        let src = ramp_source(1000, 10);
        let spec = ClipSpec {
            looped: true,
            loop_crossfade: 2,
            ..ClipSpec::new(0, 12)
        };
        let y = render_clip(&src, &spec).channels[0].clone();
        assert_eq!(&y[..8], &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        let g = |t: f64| ((t * FRAC_PI_2).sin() as f32, (t * FRAC_PI_2).cos() as f32);
        let (a, b) = g(0.25);
        assert!((y[8] - (0.0 * a + 8.0 * b)).abs() < 1e-5);
        let (a, b) = g(0.75);
        assert!((y[9] - (1.0 * a + 9.0 * b)).abs() < 1e-5);
        assert_eq!(&y[10..], &[2.0, 3.0]);
    }

    #[test]
    fn crossfades_between_clips_are_complementary() {
        let rate = 1000;
        let a = AudioBuffer::mono(rate, vec![1.0; 500]);
        let b = AudioBuffer::mono(rate, vec![1.0; 500]);
        let x = 100;
        let mut dest = AudioBuffer::silent(rate, 1, 900);
        // Clip A plays [0, 500) with a crossfade-out; clip B starts at 500 - x with a crossfade-in.
        mix_clip(
            &mut dest,
            &a,
            &ClipSpec {
                crossfade_out: x,
                ..ClipSpec::new(0, 500)
            },
            0.0,
        )
        .unwrap();
        let spec_b = ClipSpec {
            crossfade_in: x,
            ..ClipSpec::new(400, 500)
        };
        // Power complementarity: render each separately and check gA² + gB² = 1 in the overlap.
        let ra = render_clip(
            &a,
            &ClipSpec {
                crossfade_out: x,
                ..ClipSpec::new(0, 500)
            },
        );
        let rb = render_clip(&b, &spec_b);
        for m in 0..x {
            let ga = f64::from(ra.channels[0][400 + m]);
            let gb = f64::from(rb.channels[0][m]);
            assert!((ga * ga + gb * gb - 1.0).abs() < 1e-6, "overlap frame {m}");
        }
        mix_clip(&mut dest, &b, &spec_b, 0.0).unwrap();
        assert_eq!(dest.channels[0][0], 1.0);
        assert_eq!(dest.channels[0][899], 1.0);
    }

    #[test]
    fn negative_start_and_timeline_end_are_clipped() {
        let src = ramp_source(1000, 10);
        let mut dest = AudioBuffer::silent(1000, 1, 6);
        mix_clip(
            &mut dest,
            &src,
            &ClipSpec {
                fade_in: 4,
                fade_curve: FadeCurve::Linear,
                ..ClipSpec::new(-2, 10)
            },
            0.0,
        )
        .unwrap();
        // Clip frames 2.. land at timeline 0..; the fade is relative to the clip start.
        let want: Vec<f32> = (2..8)
            .map(|i| i as f32 * if i < 4 { (i as f32 + 0.5) / 4.0 } else { 1.0 })
            .collect();
        assert_eq!(dest.channels[0], want);
        // Entirely outside.
        let mut dest = AudioBuffer::silent(1000, 1, 6);
        mix_clip(&mut dest, &src, &ClipSpec::new(-20, 10), 0.0).unwrap();
        mix_clip(&mut dest, &src, &ClipSpec::new(6, 10), 0.0).unwrap();
        assert_eq!(dest.channels[0], vec![0.0; 6]);
        // Rate mismatch is an error.
        assert!(
            mix_clip(
                &mut dest,
                &AudioBuffer::mono(2000, vec![1.0]),
                &ClipSpec::new(0, 1),
                0.0
            )
            .is_err()
        );
    }

    #[test]
    fn pan_laws() {
        let (l, r) = pan_gains(0.0);
        assert!((l - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-7 && (r - l).abs() < 1e-7);
        assert_eq!(pan_gains(-1.0), (1.0, 0.0));
        let (l, r) = pan_gains(1.0);
        assert!(l.abs() < 1e-7 && (r - 1.0).abs() < 1e-7);
        for p in [-1.0f32, -0.6, -0.2, 0.0, 0.3, 0.9, 1.0] {
            let (l, r) = pan_gains(p);
            assert!((l * l + r * r - 1.0).abs() < 1e-6, "constant power at {p}");
        }
        assert_eq!(balance_gains(0.0), (1.0, 1.0));
        assert_eq!(balance_gains(-0.25), (1.0, 0.75));
        assert_eq!(balance_gains(0.5), (0.5, 1.0));
        assert_eq!(balance_gains(-3.0), (1.0, 0.0));
    }

    #[test]
    fn mix_into_channel_rules() {
        let rate = 8000;
        let mono = AudioBuffer::mono(rate, vec![1.0, 1.0]);
        let stereo = AudioBuffer::new(rate, vec![vec![1.0, 1.0], vec![0.5, 0.5]]);

        let mut d = AudioBuffer::silent(rate, 2, 3);
        mix_into(&mut d, &mono, 1, 2.0, 0.0).unwrap();
        let c = 2.0 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((d.channels[0][1] - c).abs() < 1e-6 && (d.channels[1][2] - c).abs() < 1e-6);
        assert_eq!(d.channels[0][0], 0.0);

        let mut d = AudioBuffer::silent(rate, 2, 2);
        mix_into(&mut d, &stereo, 0, 1.0, 0.5).unwrap();
        assert_eq!(d.channels, vec![vec![0.5, 0.5], vec![0.5, 0.5]]);

        let mut d = AudioBuffer::silent(rate, 1, 2);
        mix_into(&mut d, &stereo, 0, 1.0, -1.0).unwrap();
        assert_eq!(d.channels, vec![vec![0.75, 0.75]], "(L+R)/2, pan ignored");

        let mut d = AudioBuffer::silent(rate, 1, 2);
        mix_into(&mut d, &mono, -1, 1.0, 0.7).unwrap();
        assert_eq!(d.channels, vec![vec![1.0, 0.0]]);
    }

    #[test]
    fn declick() {
        let rate = 48_000;
        let mut b = AudioBuffer::new(rate, vec![vec![0.5; 1000], vec![-0.5; 1000]]);
        assert!(declick_tail(&mut b));
        assert_eq!(b.channels[0][999], 0.0);
        assert_eq!(b.channels[1][999], 0.0);
        assert!((b.channels[0][520] - 0.5 * 479.0 / 480.0).abs() < 1e-7);
        assert_eq!(b.channels[0][519], 0.5);
        let mut silent_end = AudioBuffer::mono(rate, [vec![0.5; 1000], vec![0.0; 480]].concat());
        assert!(!declick_tail(&mut silent_end));
        assert_eq!(silent_end.channels[0][999], 0.5);
        let mut short = AudioBuffer::mono(rate, vec![1.0; 10]);
        assert!(declick_tail(&mut short));
        assert_eq!(
            short.channels[0],
            (0..10).rev().map(|j| j as f32 / 10.0).collect::<Vec<_>>()
        );
        assert!(!declick_tail(&mut AudioBuffer::mono(rate, vec![])));
    }
}
