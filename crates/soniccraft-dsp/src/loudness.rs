//! EBU R128 / ITU-R BS.1770 loudness measurement (ebur128 crate).
//!
//! Channels are weighted per BS.1770 with the default map (L, R, C, LFE ignored, Ls, Rs). A mono buffer
//! is measured as a single channel with weight 1.0 — *not* as dual mono — because the constant-power
//! pan law (DESIGN.md §4.4) preserves single-channel loudness when the mono source is later spread over
//! two speakers. Values that are undefined for the input (integrated loudness of silence or of less
//! than one 400 ms gating block, peaks of digital silence) are `None`.
//!
//! The meter is streaming: it keeps O(1) state per 100 ms block, and its results do not depend on how
//! the audio is split into calls, so [`measure_with_cancel`] feeds it in blocks (polling a cancellation
//! callback in between), the limiter measures its output block by block without storing it, and
//! [`measure_file`] measures a delivered file while decoding it.

use std::path::Path;

use ebur128::{EbuR128, Mode};

use crate::analysis::linear_to_db;
use crate::decode::{self, DecodeBackend};
use crate::ffmpeg::Ffmpeg;
use crate::{AudioBuffer, DspError};

/// Frames fed to the meter per call by the block-based measurements; cancellation is polled between
/// blocks.
pub(crate) const MEASURE_BLOCK: usize = 1 << 15;

/// Source-level normalization never raises a source by more than this (DESIGN.md §4.2).
pub const MAX_NORMALIZATION_GAIN_DB: f64 = 24.0;

/// Loudness measurements of a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoudnessStats {
    /// Gated integrated loudness in LUFS; `None` for silence (everything below the −70 LUFS absolute
    /// gate) or audio shorter than 400 ms.
    pub integrated_lufs: Option<f64>,
    /// Loudness range in LU (EBU Tech 3342); `None` when the integrated loudness is undefined. Note: the
    /// ebur128 crate uses a 1 s short-term hop, ffmpeg 100 ms, so values differ between the two.
    pub loudness_range_lu: Option<f64>,
    /// Maximum true peak over all channels in dBTP (4× oversampling below 96 kHz); `None` for silence.
    pub true_peak_dbtp: Option<f64>,
    /// Maximum absolute sample over all channels in dBFS; `None` for silence.
    pub sample_peak_dbfs: Option<f64>,
}

const MODE_ALL: Mode = Mode::I
    .union(Mode::LRA)
    .union(Mode::TRUE_PEAK)
    .union(Mode::SAMPLE_PEAK);

fn loudness_err(e: ebur128::Error) -> DspError {
    DspError::Loudness(e.to_string())
}

/// `Some(v)` when `v` is finite.
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

/// A level in dB from a linear peak; `None` for 0 (or non-finite).
fn peak_db(linear: f64) -> Option<f64> {
    if linear > 0.0 {
        finite(linear_to_db(linear))
    } else {
        None
    }
}

/// Run the meter over `buffer` with `mode`.
fn meter(buffer: &AudioBuffer, mode: Mode) -> Result<EbuR128, DspError> {
    let channels = buffer.num_channels();
    if channels == 0 {
        return Err(DspError::InvalidInput(
            "cannot measure loudness of a buffer without channels".into(),
        ));
    }
    let frames = buffer.frames();
    if buffer.channels.iter().any(|c| c.len() != frames) {
        return Err(DspError::InvalidInput(
            "channels have different lengths".into(),
        ));
    }
    let channels = u32::try_from(channels).map_err(|_| {
        DspError::InvalidInput(format!("{channels} channels is too many to measure"))
    })?;
    let mut m = EbuR128::new(channels, buffer.sample_rate, mode).map_err(|e| {
        DspError::Loudness(format!(
            "cannot measure {} channels at {} Hz: {e}",
            buffer.num_channels(),
            buffer.sample_rate
        ))
    })?;
    if frames > 0 {
        let refs: Vec<&[f32]> = buffer.channels.iter().map(Vec::as_slice).collect();
        m.add_frames_planar_f32(&refs).map_err(loudness_err)?;
    }
    Ok(m)
}

fn max_over_channels(
    m: &EbuR128,
    channels: usize,
    f: fn(&EbuR128, u32) -> Result<f64, ebur128::Error>,
) -> Result<f64, DspError> {
    let mut max = 0.0f64;
    for c in 0..channels as u32 {
        max = max.max(f(m, c).map_err(loudness_err)?);
    }
    Ok(max)
}

/// A streaming meter for every value of [`LoudnessStats`], fed planar blocks of any length.
pub(crate) struct Meter {
    meter: EbuR128,
    channels: usize,
}

impl Meter {
    /// A meter for `channels` channels at `sample_rate`.
    ///
    /// Errors for 0 or more than 64 channels and for sample rates ebur128 cannot measure (outside
    /// 16 Hz – 2.8224 MHz).
    pub(crate) fn new(channels: usize, sample_rate: u32) -> Result<Self, DspError> {
        if channels == 0 {
            return Err(DspError::InvalidInput(
                "cannot measure loudness of a buffer without channels".into(),
            ));
        }
        let count = u32::try_from(channels).map_err(|_| {
            DspError::InvalidInput(format!("{channels} channels is too many to measure"))
        })?;
        let meter = EbuR128::new(count, sample_rate, MODE_ALL).map_err(|e| {
            DspError::Loudness(format!(
                "cannot measure {channels} channels at {sample_rate} Hz: {e}"
            ))
        })?;
        Ok(Meter { meter, channels })
    }

    /// Add one block: one slice per channel, all the same length.
    pub(crate) fn add(&mut self, planes: &[&[f32]]) -> Result<(), DspError> {
        if planes.first().is_some_and(|p| !p.is_empty()) {
            self.meter
                .add_frames_planar_f32(planes)
                .map_err(loudness_err)?;
        }
        Ok(())
    }

    /// The measurements of everything added so far.
    pub(crate) fn stats(&self) -> Result<LoudnessStats, DspError> {
        let m = &self.meter;
        let integrated_lufs = finite(m.loudness_global().map_err(loudness_err)?);
        let loudness_range_lu = match integrated_lufs {
            Some(_) => finite(m.loudness_range().map_err(loudness_err)?),
            None => None,
        };
        Ok(LoudnessStats {
            integrated_lufs,
            loudness_range_lu,
            true_peak_dbtp: peak_db(max_over_channels(m, self.channels, EbuR128::true_peak)?),
            sample_peak_dbfs: peak_db(max_over_channels(m, self.channels, EbuR128::sample_peak)?),
        })
    }
}

/// Integrated loudness, loudness range, true peak and sample peak in one pass.
///
/// Errors when the buffer has no channels, ragged channels, more than 64 channels or a sample rate
/// outside 16 Hz – 2.8224 MHz.
pub fn measure(buffer: &AudioBuffer) -> Result<LoudnessStats, DspError> {
    measure_with_cancel(buffer, &|| false)
}

/// [`measure`], polling `cancel` between blocks of the buffer: it returns [`DspError::Cancelled`] soon
/// after `cancel` returns `true`.
pub fn measure_with_cancel(
    buffer: &AudioBuffer,
    cancel: &dyn Fn() -> bool,
) -> Result<LoudnessStats, DspError> {
    let frames = buffer.frames();
    if buffer.channels.iter().any(|c| c.len() != frames) {
        return Err(DspError::InvalidInput(
            "channels have different lengths".into(),
        ));
    }
    let mut meter = Meter::new(buffer.num_channels(), buffer.sample_rate)?;
    let mut start = 0;
    while start < frames {
        if cancel() {
            return Err(DspError::Cancelled);
        }
        let end = (start + MEASURE_BLOCK).min(frames);
        let planes: Vec<&[f32]> = buffer.channels.iter().map(|c| &c[start..end]).collect();
        meter.add(&planes)?;
        start = end;
    }
    meter.stats()
}

/// The loudness of an audio file, measured on its decoded audio (see [`measure_file`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileMeasurement {
    /// Gated integrated loudness in LUFS (`None` when undefined).
    pub integrated_lufs: Option<f64>,
    /// Maximum true peak over all channels in dBTP, on the BS.1770 4× meter (`None` for silence).
    pub true_peak_dbtp: Option<f64>,
    /// Maximum absolute sample over all channels in dBFS (`None` for silence).
    pub sample_peak_dbfs: Option<f64>,
    /// Loudness range in LU (`None` when the integrated loudness is undefined).
    pub loudness_range_lu: Option<f64>,
    /// Decoded frames.
    pub frames: u64,
    /// Sample rate of the decoded audio in Hz (48000 for Opus).
    pub sample_rate: u32,
    /// Channels of the decoded audio.
    pub channels: usize,
    /// The decoder that read the file.
    pub backend: DecodeBackend,
}

/// Measure a delivered file the way the master is measured: decode it and run the same BS.1770 meter
/// (integrated loudness, true peak on the 4× meter, sample peak, loudness range) over the decoded audio.
/// Lossy codecs reshape peaks (and shift the loudness slightly), so this is how an mp3, ogg, opus or m4a
/// output is checked against the true-peak ceiling.
///
/// The decode streams: nothing proportional to the file's length is kept. symphonia reads wav, flac,
/// mp3, ogg vorbis and m4a/aac (gapless trims and the MP4 edit list applied, exactly as
/// [`crate::decode::decode_file_with`] reads them); anything else (opus) goes through ffmpeg, located
/// per `ffmpeg`. Channels are measured as the file has them (no downmix).
///
/// Errors: I/O, undecodable files and unsupported sample rates ([`DspError::Decode`]).
pub fn measure_file(path: &Path, ffmpeg: &Ffmpeg) -> Result<FileMeasurement, DspError> {
    measure_file_with_cancel(path, ffmpeg, &|| false)
}

/// [`measure_file`], polling `cancel` while decoding ([`DspError::Cancelled`]).
pub fn measure_file_with_cancel(
    path: &Path,
    ffmpeg: &Ffmpeg,
    cancel: &dyn Fn() -> bool,
) -> Result<FileMeasurement, DspError> {
    let mut sink = MeterSink { meter: None };
    let summary = decode::stream_file(path, ffmpeg, &mut sink, cancel)?;
    let stats = match &sink.meter {
        Some(meter) => meter.stats()?,
        None => LoudnessStats::default(),
    };
    Ok(FileMeasurement {
        integrated_lufs: stats.integrated_lufs,
        true_peak_dbtp: stats.true_peak_dbtp,
        sample_peak_dbfs: stats.sample_peak_dbfs,
        loudness_range_lu: stats.loudness_range_lu,
        frames: summary.frames,
        sample_rate: summary.sample_rate,
        channels: summary.channels,
        backend: summary.backend,
    })
}

/// Feeds a streaming decode into a [`Meter`].
struct MeterSink {
    meter: Option<Meter>,
}

impl decode::StreamSink for MeterSink {
    fn start(&mut self, sample_rate: u32, channels: usize) -> Result<(), DspError> {
        self.meter = Some(Meter::new(channels, sample_rate)?);
        Ok(())
    }

    fn frames(&mut self, planes: &[&[f32]]) -> Result<(), DspError> {
        match &mut self.meter {
            Some(meter) => meter.add(planes),
            None => Err(DspError::InvalidInput(
                "audio arrived before its layout".into(),
            )),
        }
    }

    fn reset(&mut self) {
        self.meter = None;
    }
}

/// Gated integrated loudness in LUFS only (cheaper than [`measure`]); `None` when undefined.
pub fn integrated_lufs(buffer: &AudioBuffer) -> Result<Option<f64>, DspError> {
    let m = meter(buffer, Mode::I)?;
    Ok(finite(m.loudness_global().map_err(loudness_err)?))
}

/// Maximum true peak over all channels, linear (1.0 = 0 dBTP), measured by ebur128 with 4×
/// oversampling (an independent estimator from the limiter's).
pub fn true_peak(buffer: &AudioBuffer) -> Result<f64, DspError> {
    let m = meter(buffer, Mode::TRUE_PEAK)?;
    max_over_channels(&m, buffer.num_channels(), EbuR128::true_peak)
}

/// [`true_peak`] in dBTP; `None` for silence.
pub fn true_peak_dbtp(buffer: &AudioBuffer) -> Result<Option<f64>, DspError> {
    Ok(peak_db(true_peak(buffer)?))
}

/// Gain in dB that brings a source measured at `integrated` LUFS to `target_lufs`, capped at
/// [`MAX_NORMALIZATION_GAIN_DB`] (DESIGN.md §4.2). `None` when the loudness is undefined (the source is
/// then left unchanged, `normalization_skipped`).
pub fn normalization_gain_db(integrated: Option<f64>, target_lufs: f64) -> Option<f64> {
    let measured = integrated.filter(|v| v.is_finite())?;
    Some((target_lufs - measured).min(MAX_NORMALIZATION_GAIN_DB))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, secs: f64, freq: f64, amp: f64) -> Vec<f32> {
        let n = (secs * f64::from(rate)).round() as usize;
        (0..n)
            .map(|i| {
                (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(rate)).sin())
                    as f32
            })
            .collect()
    }

    #[test]
    fn sine_reference_levels() {
        // 1 kHz at -20 dBFS peak: stereo reads -20 LUFS (BS.1770 calibration), mono -23 LUFS.
        let amp = 0.1;
        let tone = sine(48_000, 5.0, 1000.0, amp);
        let stereo = AudioBuffer::new(48_000, vec![tone.clone(), tone.clone()]);
        let s = measure(&stereo).unwrap();
        let i = s.integrated_lufs.unwrap();
        assert!((i + 20.0).abs() < 0.05, "stereo {i}");
        let mono = measure(&AudioBuffer::mono(48_000, tone)).unwrap();
        let i = mono.integrated_lufs.unwrap();
        assert!((i + 23.01).abs() < 0.05, "mono {i}");
        assert!((s.true_peak_dbtp.unwrap() + 20.0).abs() < 0.05);
        assert!((s.sample_peak_dbfs.unwrap() + 20.0).abs() < 0.01);
        assert!(
            s.loudness_range_lu.unwrap().abs() < 0.1,
            "steady tone has ~0 LRA"
        );
        assert!(
            (integrated_lufs(&stereo).unwrap().unwrap() - s.integrated_lufs.unwrap()).abs() < 1e-9
        );
    }

    #[test]
    fn undefined_values_are_none() {
        let silent = measure(&AudioBuffer::silent(48_000, 2, 96_000)).unwrap();
        assert_eq!(silent, LoudnessStats::default());
        let short = measure(&AudioBuffer::mono(48_000, sine(48_000, 0.3, 1000.0, 0.5))).unwrap();
        assert_eq!(short.integrated_lufs, None, "shorter than one gating block");
        assert!(short.true_peak_dbtp.is_some());
        let empty = measure(&AudioBuffer::mono(48_000, vec![])).unwrap();
        assert_eq!(empty, LoudnessStats::default());
        assert!(measure(&AudioBuffer::new(48_000, vec![])).is_err());
        assert!(measure(&AudioBuffer::mono(8, vec![0.0])).is_err());
        assert_eq!(
            true_peak_dbtp(&AudioBuffer::silent(44_100, 1, 100)).unwrap(),
            None
        );
    }

    #[test]
    fn true_peak_sees_intersample_overs() {
        // fs/4 sine at 45° phase: samples at ±0.707·A, true peak A.
        let rate = 48_000;
        let a = 0.9f64;
        let x: Vec<f32> = (0..rate as usize)
            .map(|i| {
                (a * (std::f64::consts::FRAC_PI_2 * i as f64 + std::f64::consts::FRAC_PI_4).sin())
                    as f32
            })
            .collect();
        let buf = AudioBuffer::mono(rate, x);
        let tp = true_peak(&buf).unwrap();
        assert!(f64::from(buf.peak()) < 0.65);
        assert!(
            tp > 0.85,
            "true peak {tp} should reveal the inter-sample peak"
        );
    }

    #[test]
    fn normalization_gain() {
        assert_eq!(normalization_gain_db(Some(-30.0), -20.0), Some(10.0));
        assert_eq!(normalization_gain_db(Some(-10.0), -20.0), Some(-10.0));
        assert_eq!(normalization_gain_db(Some(-60.0), -20.0), Some(24.0));
        assert_eq!(normalization_gain_db(None, -20.0), None);
        assert_eq!(normalization_gain_db(Some(f64::NEG_INFINITY), -20.0), None);
    }
}
