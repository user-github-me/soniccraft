//! Track stems, ducking and the master sum (DESIGN.md §4.3–§4.6.1).
//!
//! Every track's items are first summed *pre-fader* (cue/clip gains, fades, loops; no pan, no track gain)
//! into a mono accumulator (mono items) and a stereo accumulator (stereo clips). That pre-fader content is
//! what sidechain ducking listens to, mixed to mono (`(L + R)/2`). The stem is the pre-fader content panned
//! (constant power for mono items, balance for stereo ones) times the track gain (0 when muted), then
//! ducked. The master is the sum of the stems in declaration order.
//!
//! # Memory
//!
//! The master is the only buffer as long as the timeline. Tracks are processed one after another, each in
//! windows of [`WINDOW`] frames: a window of its pre-fader content is rendered from the processed cues and
//! decoded sources, panned, gained and ducked into a window of its stem, streamed to the stem file (when
//! stems are exported) and added to the master. Sidechain ducking first needs the trigger content of the
//! whole timeline: one mono detector, built window by window the same way, exists (alone) before the master
//! is allocated, and its activity is measured without copying it. Every sample is computed with the same
//! operations in the same order as on whole-timeline buffers, so the result does not depend on the window
//! size.

use std::fs::File;
use std::io::BufWriter;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::{codes, display_path};
use soniccraft_core::ir::{ClipIr, DuckingMode, FadeCurve, NodeKind, Program, TrackIdx};
use soniccraft_dsp::duck::{Activity, DETECTOR_WINDOW_MS, DuckEnvelope};
use soniccraft_dsp::render::{self, ClipSpec};
use soniccraft_dsp::{AudioBuffer, DspError};
use soniccraft_layout::{Layout, Placement, PlacementKind};

use super::sources::SourceBank;
use crate::cancel::{Cancel, Stop};
use crate::files::Staging;
use crate::reports;

/// Frames rendered per step of a track.
pub(crate) const WINDOW: usize = 1 << 16;

/// Inputs of the mixing stage.
pub(crate) struct MixJob {
    /// The program.
    pub program: Arc<Program>,
    /// Its exact layout.
    pub layout: Arc<Layout>,
    /// Processed cue audio by node id.
    pub cues: Vec<Option<AudioBuffer>>,
    /// Decoded clip sources.
    pub sources: Arc<SourceBank>,
    /// Linear gain of every clip (clip gain × source normalization) by node id.
    pub clip_gains: Vec<f32>,
    /// Stem file of every track (index = track), when stems are exported.
    pub stems: Option<Vec<PathBuf>>,
    /// Where stem files are written until the render commits.
    pub staging: Arc<Mutex<Staging>>,
    /// Sample the ducking envelopes for the preview.
    pub envelopes: bool,
    /// Cancellation.
    pub cancel: Cancel,
}

/// Ducking of one track.
#[derive(Debug)]
pub(crate) struct Ducked {
    /// The ducked track.
    pub track: TrackIdx,
    /// Active regions in frames.
    pub regions: Vec<Range<usize>>,
    /// Gain in dB at 50 points per second (when requested).
    pub envelope: Option<Vec<f64>>,
}

/// Outputs of the mixing stage.
#[derive(Debug)]
pub(crate) struct Mixed {
    /// Sum of the stems (pre-master).
    pub master: AudioBuffer,
    /// Every ducked track, in declaration order.
    pub ducking: Vec<Ducked>,
    /// Written stems: track id and (final) path.
    pub stems: Vec<(String, PathBuf)>,
}

/// Why mixing stopped.
pub(crate) enum MixError {
    /// Cancelled or an internal failure.
    Stop(Stop),
    /// Writing a stem failed, or a buffer could not be allocated.
    Diagnostic(Box<Diagnostic>),
}

impl From<Stop> for MixError {
    fn from(stop: Stop) -> Self {
        MixError::Stop(stop)
    }
}

impl From<DspError> for MixError {
    fn from(error: DspError) -> Self {
        MixError::Stop(Stop::failed(format!(
            "internal error while mixing: {error}"
        )))
    }
}

impl MixJob {
    /// Run the stage (blocking).
    pub(crate) fn run(self) -> Result<Mixed, MixError> {
        let program = &self.program;
        let sample_rate = program.output.sample_rate;
        let channels = usize::from(program.output.channels).max(1);
        let total = usize::try_from(self.layout.total_frames).unwrap_or(usize::MAX);

        // 1. Ducking envelopes. Sidechain detectors are built one at a time, before the master exists.
        let mut envelopes: Vec<Option<DuckEnvelope>> = vec![None; program.tracks.len()];
        for (index, track) in program.tracks.iter().enumerate() {
            let Some(config) = &track.ducking else {
                continue;
            };
            self.cancel.check()?;
            let triggers = reports::active_triggers(program, config);
            let params = reports::duck_params(config);
            let envelope = match config.mode {
                DuckingMode::Cue => DuckEnvelope::cue(
                    &reports::trigger_intervals(&self.layout, &triggers),
                    sample_rate,
                    total,
                    &params,
                ),
                DuckingMode::Sidechain => {
                    let detector = self.detector(&triggers, total)?;
                    let activity =
                        sidechain_activity(&detector, sample_rate, params.threshold_db, total);
                    drop(detector);
                    DuckEnvelope::new(
                        activity.hold(params.hold_ms).lookahead(params.lookahead_ms),
                        &params,
                    )
                }
            };
            envelopes[index] = Some(envelope);
        }

        // 2. Stems, ducked, summed in declaration order.
        let mut master = allocate(sample_rate, channels, total)?;
        let mut stems = Vec::new();
        let mut ducking = Vec::new();
        for (index, track) in program.tracks.iter().enumerate() {
            self.cancel.check()?;
            if let Some(envelope) = &envelopes[index] {
                ducking.push(Ducked {
                    track: index,
                    regions: envelope.regions(),
                    envelope: self
                        .envelopes
                        .then(|| envelope.sample_gain_db(crate::preview::POINTS_PER_SECOND)),
                });
            }
            let stem_file = self.stems.as_ref().and_then(|files| files.get(index));
            // A muted track is silent: nothing to render unless its (silent) stem is exported.
            if track.mute && stem_file.is_none() {
                continue;
            }
            let mut writer = match stem_file {
                Some(path) => Some(StemWriter::create(
                    &self.staging,
                    path,
                    channels,
                    sample_rate,
                    &track.id,
                )?),
                None => None,
            };
            let gain = track.gain as f32;
            let audible = !track.mute && gain != 0.0;
            // DuckEnvelope::apply leaves a stem alone when the duck is never engaged.
            let mut duck = envelopes[index]
                .as_ref()
                .filter(|e| e.depth_db() != 0.0 && !e.activity().is_silent())
                .map(Duck::new);
            // Every window when a stem file or a duck envelope must follow the whole timeline, else only
            // the windows the track's items touch.
            let range = if writer.is_some() || duck.is_some() {
                0..total
            } else if audible {
                self.track_span(index, total)
            } else {
                0..0
            };
            for window in windows(range) {
                self.cancel.check()?;
                let mut stem = if audible {
                    self.stem_window(index, &window, channels, gain, track.pan as f32)?
                } else {
                    None
                };
                if let Some(duck) = &mut duck {
                    duck.apply(stem.as_mut(), window.len());
                }
                if let Some(writer) = &mut writer {
                    writer.write(stem.as_ref(), window.len())?;
                }
                if let Some(stem) = &stem {
                    render::mix_into(&mut master, stem, window.start as i64, 1.0, 0.0)?;
                }
            }
            if let (Some(writer), Some(path)) = (writer, stem_file) {
                writer.finish()?;
                stems.push((track.id.clone(), path.clone()));
            }
        }
        Ok(Mixed {
            master,
            ducking,
            stems,
        })
    }

    /// The sidechain detector of a ducked track: the mono mix `(L + R)/2` of its triggers' pre-fader
    /// content over the whole timeline.
    fn detector(&self, triggers: &[TrackIdx], total: usize) -> Result<AudioBuffer, MixError> {
        let sample_rate = self.program.output.sample_rate;
        let mut detector = allocate(sample_rate, 1, total)?;
        for &trigger in triggers {
            for window in windows(self.track_span(trigger, total)) {
                self.cancel.check()?;
                let content = self.content_window(trigger, &window)?;
                if content.is_empty() {
                    continue;
                }
                let mut mono = AudioBuffer::silent(sample_rate, 1, window.len());
                for part in [&content.mono, &content.stereo].into_iter().flatten() {
                    render::mix_into(&mut mono, part, 0, 1.0, 0.0)?;
                }
                render::mix_into(&mut detector, &mono, window.start as i64, 1.0, 0.0)?;
            }
        }
        Ok(detector)
    }

    /// One window of a track's stem (pre-fader content panned and gained; not yet ducked), or None when
    /// none of its items plays in the window.
    fn stem_window(
        &self,
        track: TrackIdx,
        window: &Range<usize>,
        channels: usize,
        gain: f32,
        pan: f32,
    ) -> Result<Option<AudioBuffer>, MixError> {
        let content = self.content_window(track, window)?;
        if content.is_empty() {
            return Ok(None);
        }
        let mut stem = AudioBuffer::silent(self.program.output.sample_rate, channels, window.len());
        for part in [&content.mono, &content.stereo].into_iter().flatten() {
            render::mix_into(&mut stem, part, 0, gain, pan)?;
        }
        Ok(Some(stem))
    }

    /// Frames `[start, end)` of a track's pre-fader content (items in declaration order).
    fn content_window(&self, track: TrackIdx, window: &Range<usize>) -> Result<Content, MixError> {
        let program = &self.program;
        let sample_rate = program.output.sample_rate;
        let mono_only = program.output.channels == 1;
        let frames = window.len();
        let offset = window.start as i64;
        let mut content = Content::default();
        let Some(t) = program.tracks.get(track) else {
            return Ok(content);
        };
        for &node in &t.nodes {
            let Some(placement) = self.layout.placements.get(node) else {
                continue;
            };
            if placement.kind == PlacementKind::Pause || !overlaps(placement, window) {
                continue;
            }
            match &program.nodes[node].kind {
                NodeKind::Cue(_) => {
                    if let Some(Some(audio)) = self.cues.get(node) {
                        let acc = content
                            .mono
                            .get_or_insert_with(|| AudioBuffer::silent(sample_rate, 1, frames));
                        render::mix_into(acc, audio, placement.start - offset, 1.0, 0.0)?;
                    }
                }
                NodeKind::Clip(clip) => {
                    let Some(source) = self.sources.get(&clip.source_path) else {
                        continue;
                    };
                    let gain = self.clip_gains.get(node).copied().unwrap_or(1.0);
                    let mut spec = clip_spec(placement, clip, gain);
                    spec.start -= offset;
                    let acc = if mono_only || source.buffer.num_channels() == 1 {
                        content
                            .mono
                            .get_or_insert_with(|| AudioBuffer::silent(sample_rate, 1, frames))
                    } else {
                        content
                            .stereo
                            .get_or_insert_with(|| AudioBuffer::silent(sample_rate, 2, frames))
                    };
                    render::mix_clip(acc, &source.buffer, &spec, 0.0)?;
                }
                NodeKind::Pause { .. } => {}
            }
        }
        Ok(content)
    }

    /// The frames a track's items play in, within `[0, total)` (empty when it has none).
    fn track_span(&self, track: TrackIdx, total: usize) -> Range<usize> {
        let mut span: Option<(i64, i64)> = None;
        if let Some(t) = self.program.tracks.get(track) {
            for &node in &t.nodes {
                let Some(p) = self.layout.placements.get(node) else {
                    continue;
                };
                if p.kind == PlacementKind::Pause || p.length == 0 {
                    continue;
                }
                span = Some(match span {
                    None => (p.start, p.end()),
                    Some((s, e)) => (s.min(p.start), e.max(p.end())),
                });
            }
        }
        let Some((start, end)) = span else {
            return 0..0;
        };
        let clamp = |f: i64| usize::try_from(f.max(0)).unwrap_or(usize::MAX).min(total);
        let (start, end) = (clamp(start), clamp(end));
        if start < end { start..end } else { 0..0 }
    }
}

/// True when `placement` plays at least one frame of `window`.
fn overlaps(placement: &Placement, window: &Range<usize>) -> bool {
    let (start, end) = (window.start as i64, window.end as i64);
    placement.length > 0 && placement.start < end && placement.end() > start
}

/// `range` cut into windows of [`WINDOW`] frames that start at multiples of [`WINDOW`] (so a track's
/// windows line up with every other track's).
fn windows(range: Range<usize>) -> impl Iterator<Item = Range<usize>> {
    let first = range.start / WINDOW;
    let end = range.end;
    (first..)
        .map(|w| w * WINDOW)
        .take_while(move |&start| start < end)
        .map(move |start| start.max(range.start)..(start + WINDOW).min(end))
        .filter(|window| window.start < window.end)
}

/// A track's pre-fader content in one window.
#[derive(Default)]
struct Content {
    /// Sum of the mono items (and of everything, for mono output).
    mono: Option<AudioBuffer>,
    /// Sum of the stereo clips.
    stereo: Option<AudioBuffer>,
}

impl Content {
    fn is_empty(&self) -> bool {
        self.mono.is_none() && self.stereo.is_none()
    }
}

/// A duck envelope applied window by window: exactly `DuckEnvelope::apply` on the whole stem.
struct Duck<'a> {
    gains: Box<dyn Iterator<Item = f64> + 'a>,
    depth: f64,
    full: f32,
}

impl<'a> Duck<'a> {
    fn new(envelope: &'a DuckEnvelope) -> Self {
        let depth = envelope.depth_db();
        Duck {
            gains: Box::new(envelope.gains_db()),
            depth,
            full: (10f64.powf(-depth / 20.0)) as f32,
        }
    }

    /// Duck the next `frames` frames: `stem` (when the track plays in this window) is multiplied by the
    /// envelope; the envelope advances either way.
    fn apply(&mut self, stem: Option<&mut AudioBuffer>, frames: usize) {
        let Some(stem) = stem else {
            for _ in (&mut self.gains).take(frames) {}
            return;
        };
        for i in 0..frames {
            let g_db = self.gains.next().unwrap_or(0.0);
            let g = if g_db == 0.0 {
                continue;
            } else if g_db == -self.depth {
                self.full
            } else {
                10f64.powf(g_db / 20.0) as f32
            };
            for ch in &mut stem.channels {
                ch[i] *= g;
            }
        }
    }
}

/// Activity of a sidechain detector: exactly [`Activity::from_sidechain`] (DESIGN.md §4.5 step 1: RMS over
/// a 10 ms window centred on each 1 ms block, active above `threshold_db`), streamed over the detector
/// instead of building a copy and a prefix-sum array as long as the timeline.
pub(crate) fn sidechain_activity(
    detector: &AudioBuffer,
    sample_rate: u32,
    threshold_db: f64,
    frames: usize,
) -> Activity {
    let blocks = block_count(sample_rate, frames);
    let Some(x) = detector.channels.first().filter(|_| blocks > 0) else {
        return Activity::inactive(sample_rate, frames);
    };
    let x = &x[..x.len().min(frames)];
    let window = ((DETECTOR_WINDOW_MS * f64::from(sample_rate) / 1000.0).round() as usize).max(1);
    let threshold = 10f64.powf(threshold_db / 10.0);
    // Two running prefix sums of x² (Σ x[..k]² accumulated in order, as from_sidechain does), one for each
    // edge of the window; both edges only move forward.
    let mut lo_sum = PrefixSum::new(x);
    let mut hi_sum = PrefixSum::new(x);
    let mut runs: Vec<Range<usize>> = Vec::new();
    let mut run_start: Option<usize> = None;
    for b in 0..blocks {
        let start = block_start(sample_rate, b).min(frames);
        let end = block_start(sample_rate, b + 1).min(frames);
        let centre = (start + end) / 2;
        let lo = centre.saturating_sub(window / 2);
        let hi = (lo + window).min(frames);
        let (lo_c, hi_c) = (lo.min(x.len()), hi.min(x.len()));
        let count = hi - lo;
        let active = count > 0
            && hi_c > lo_c
            && (hi_sum.at(hi_c) - lo_sum.at(lo_c)) / count as f64 > threshold;
        match (active, run_start) {
            (true, None) => run_start = Some(b),
            (false, Some(first)) => {
                runs.push(block_start(sample_rate, first).min(frames)..start);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(first) = run_start {
        runs.push(block_start(sample_rate, first).min(frames)..frames);
    }
    Activity::from_intervals(&runs, sample_rate, frames)
}

/// `Σ x[..k]²` in f64 for nondecreasing `k`, accumulated in index order.
struct PrefixSum<'a> {
    x: &'a [f32],
    k: usize,
    sum: f64,
}

impl<'a> PrefixSum<'a> {
    fn new(x: &'a [f32]) -> Self {
        PrefixSum { x, k: 0, sum: 0.0 }
    }

    fn at(&mut self, k: usize) -> f64 {
        while self.k < k {
            let s = f64::from(self.x[self.k]);
            self.sum += s * s;
            self.k += 1;
        }
        self.sum
    }
}

/// Number of 1 ms blocks covering `frames` frames (as soniccraft_dsp::duck counts them).
fn block_count(sample_rate: u32, frames: usize) -> usize {
    if sample_rate == 0 {
        return 0;
    }
    (frames as u128 * 1000).div_ceil(u128::from(sample_rate)) as usize
}

/// First frame of 1 ms block `b` (as soniccraft_dsp::duck numbers them).
fn block_start(sample_rate: u32, b: usize) -> usize {
    (b as u128 * u128::from(sample_rate) / 1000) as usize
}

/// A silent buffer as long as the timeline, or a diagnostic when the memory cannot be had (instead of an
/// abort).
fn allocate(sample_rate: u32, channels: usize, frames: usize) -> Result<AudioBuffer, MixError> {
    let mut planes = Vec::with_capacity(channels);
    for _ in 0..channels {
        let mut plane: Vec<f32> = Vec::new();
        if plane.try_reserve_exact(frames).is_err() {
            let gb = (frames as f64 * channels as f64 * 4.0) / f64::from(1u32 << 30);
            return Err(MixError::Diagnostic(Box::new(
                Diagnostic::error(
                    codes::TIMELINE_TOO_LONG,
                    format!(
                        "this machine could not provide the {gb:.1} GB a {}-frame mix needs",
                        frames
                    ),
                )
                .with_hint(
                    "Split the timeline into several shorter timelines and render them separately, or free \
                     memory and render again",
                ),
            )));
        }
        plane.resize(frames, 0.0);
        planes.push(plane);
    }
    Ok(AudioBuffer::new(sample_rate, planes))
}

/// A stem file written window by window (32-bit float WAV, the layout `soniccraft_dsp::wav::write_f32`
/// writes), to its temporary name until the render commits.
struct StemWriter {
    writer: hound::WavWriter<BufWriter<File>>,
    channels: usize,
    track: String,
}

impl StemWriter {
    fn create(
        staging: &Mutex<Staging>,
        target: &std::path::Path,
        channels: usize,
        sample_rate: u32,
        track: &str,
    ) -> Result<Self, MixError> {
        let fail = |e: &dyn std::fmt::Display| stem_error(track, target, e);
        let mut staging = staging.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(dir) = target.parent() {
            staging.create_dir_all(dir).map_err(|e| fail(&e))?;
        }
        let (_, file) = staging.create(target).map_err(|e| fail(&e))?;
        let spec = hound::WavSpec {
            channels: u16::try_from(channels).unwrap_or(2),
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::new(BufWriter::with_capacity(1 << 16, file), spec)
            .map_err(|e| fail(&e))?;
        Ok(StemWriter {
            writer,
            channels,
            track: track.to_string(),
        })
    }

    /// Append `frames` frames: `stem`, or silence when the track does not play in this window.
    fn write(&mut self, stem: Option<&AudioBuffer>, frames: usize) -> Result<(), MixError> {
        let track = &self.track;
        let fail = |e: hound::Error| {
            MixError::Diagnostic(Box::new(
                Diagnostic::error(
                    codes::ENCODE_FAILED,
                    format!("cannot write the stem of track `{track}`: {e}"),
                )
                .with_hint("Check that the output directory is writable and the disk is not full"),
            ))
        };
        for i in 0..frames {
            for c in 0..self.channels {
                let s = stem.map_or(0.0, |b| b.channels[c][i]);
                self.writer
                    .write_sample(if s.is_finite() { s } else { 0.0 })
                    .map_err(fail)?;
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<(), MixError> {
        let track = self.track;
        self.writer.finalize().map_err(|e| {
            MixError::Diagnostic(Box::new(
                Diagnostic::error(
                    codes::ENCODE_FAILED,
                    format!("cannot write the stem of track `{track}`: {e}"),
                )
                .with_hint("Check that the output directory is writable and the disk is not full"),
            ))
        })
    }
}

fn stem_error(track: &str, target: &std::path::Path, error: &dyn std::fmt::Display) -> MixError {
    MixError::Diagnostic(Box::new(
        Diagnostic::error(
            codes::ENCODE_FAILED,
            format!(
                "cannot write the stem of track `{track}` ({}): {error}",
                display_path(&target.display().to_string())
            ),
        )
        .with_hint("Check that the output directory is writable and the disk is not full"),
    ))
}

/// How the layout's placement of a clip plays its source. A fade whose curve differs from the clip's
/// `fade_curve` comes from a crossfade (always equal power) and is rendered as one.
pub(crate) fn clip_spec(placement: &Placement, clip: &ClipIr, gain: f32) -> ClipSpec {
    let curve = fade_curve(clip.fade_curve);
    let own_in = fade_curve(placement.fade_in_curve) == curve;
    let own_out = fade_curve(placement.fade_out_curve) == curve;
    let frames = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
    ClipSpec {
        start: placement.start,
        length: frames(placement.length),
        source_offset: frames(placement.trim_start),
        looped: placement.looped,
        loop_crossfade: frames(placement.loop_crossfade),
        fade_in: if own_in { frames(placement.fade_in) } else { 0 },
        fade_out: if own_out {
            frames(placement.fade_out)
        } else {
            0
        },
        fade_curve: curve,
        crossfade_in: if own_in { 0 } else { frames(placement.fade_in) },
        crossfade_out: if own_out {
            0
        } else {
            frames(placement.fade_out)
        },
        gain,
    }
}

fn fade_curve(curve: FadeCurve) -> render::FadeCurve {
    match curve {
        FadeCurve::Linear => render::FadeCurve::Linear,
        FadeCurve::EqualPower => render::FadeCurve::EqualPower,
        FadeCurve::Exponential => render::FadeCurve::Exponential,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soniccraft_core::ir::ClipLength;
    use soniccraft_layout::DurationSource;

    fn clip(curve: FadeCurve) -> ClipIr {
        ClipIr {
            source: "a.wav".into(),
            source_path: "/a.wav".into(),
            trim_start_ms: 0.0,
            length: ClipLength::Natural,
            looped: false,
            loop_crossfade_ms: 0.0,
            fade_in_ms: 0.0,
            fade_out_ms: 0.0,
            fade_curve: curve,
            crossfade_ms: 0.0,
            gain: 1.0,
            shorthand: false,
        }
    }

    fn placement(in_curve: FadeCurve, out_curve: FadeCurve) -> Placement {
        Placement {
            node: 0,
            track: 0,
            kind: PlacementKind::Clip,
            start: 100,
            length: 1000,
            open_ended: false,
            looped: true,
            pushed_from: None,
            duration_source: DurationSource::Source,
            trim_start: 7,
            fade_in: 50,
            fade_out: 60,
            fade_in_curve: in_curve,
            fade_out_curve: out_curve,
            loop_crossfade: 9,
        }
    }

    #[test]
    fn crossfade_fades_render_equal_power() {
        let spec = clip_spec(
            &placement(FadeCurve::EqualPower, FadeCurve::Linear),
            &clip(FadeCurve::Linear),
            0.5,
        );
        assert_eq!((spec.fade_in, spec.crossfade_in), (0, 50));
        assert_eq!((spec.fade_out, spec.crossfade_out), (60, 0));
        assert_eq!(spec.fade_curve, render::FadeCurve::Linear);
        assert_eq!(
            (spec.start, spec.length, spec.source_offset),
            (100, 1000, 7)
        );
        assert!(spec.looped && spec.loop_crossfade == 9 && spec.gain == 0.5);

        let own = clip_spec(
            &placement(FadeCurve::EqualPower, FadeCurve::EqualPower),
            &clip(FadeCurve::EqualPower),
            1.0,
        );
        assert_eq!(
            (
                own.fade_in,
                own.fade_out,
                own.crossfade_in,
                own.crossfade_out
            ),
            (50, 60, 0, 0)
        );
    }

    #[test]
    fn windows_line_up_on_multiples_of_the_window() {
        let w: Vec<Range<usize>> = windows(10..(2 * WINDOW + 5)).collect();
        assert_eq!(
            w,
            [10..WINDOW, WINDOW..2 * WINDOW, 2 * WINDOW..2 * WINDOW + 5]
        );
        assert_eq!(windows(0..0).count(), 0);
        assert_eq!(windows(5..5).count(), 0);
        assert_eq!(windows(100..100).count(), 0);
        let one: Vec<Range<usize>> = windows(WINDOW + 3..WINDOW + 9).collect();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0], WINDOW + 3..WINDOW + 9);
    }

    /// Deterministic noise bursts with silences, so detector levels cross the threshold often.
    fn detector(frames: usize, seed: u64) -> AudioBuffer {
        let mut state = seed | 1;
        let samples = (0..frames)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let noise = ((state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32;
                let burst = ((i / 997) % 3 != 0) as u8 as f32;
                noise * burst * (0.001 + 0.2 * ((i / 4093) % 5) as f32 / 4.0)
            })
            .collect();
        AudioBuffer::mono(48_000, samples)
    }

    #[test]
    fn streamed_sidechain_activity_equals_the_dsp_detector() {
        for (rate, frames, threshold, len) in [
            (48_000u32, 96_000usize, -40.0, 96_000usize),
            (44_100, 70_001, -30.0, 70_001),
            (22_050, 33_333, -50.0, 30_000),
            (8_000, 12_345, -35.0, 12_345),
            (48_000, 4_800, -40.0, 0),
            (48_000, 1, -40.0, 1),
        ] {
            let mut buffer = detector(len, u64::from(rate) + frames as u64);
            buffer.sample_rate = rate;
            let expected = Activity::from_sidechain(&buffer, threshold, frames);
            let streamed = sidechain_activity(&buffer, rate, threshold, frames);
            assert_eq!(streamed, expected, "{rate} Hz, {frames} frames");
            assert!(
                expected.regions().len() > 1 || len < 10_000,
                "the test signal crosses the threshold: {:?}",
                expected.regions()
            );
        }
    }
}
