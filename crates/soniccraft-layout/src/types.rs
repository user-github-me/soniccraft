//! Public data types: the duration table that goes in and the [`Layout`] that comes out.
//!
//! Everything is measured in sample frames at the output sample rate (DESIGN.md §0.4). Milliseconds only
//! appear in the convenience accessors, which convert with [`soniccraft_core::ir::frames_to_ms`].

use serde::{Deserialize, Serialize};
use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics;
use soniccraft_core::ir::{FadeCurve, NodeId, TrackIdx, frames_to_ms};

/// Where a node's duration came from.
///
/// The variants and their serialized names mirror the engine's report `DurationSource` one to one, so the
/// engine converts with a plain `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationSource {
    /// Measured from audio synthesized in this run.
    Exact,
    /// Measured from cached audio.
    Cache,
    /// Heuristic estimate (dry run), or a source whose length could not be determined.
    Estimate,
    /// Measured from the clip's source file.
    Source,
    /// Fixed by the document (pauses, explicit clip lengths, open-ended loops).
    Fixed,
}

impl DurationSource {
    /// The serialized name (`exact`, `cache`, `estimate`, `source`, `fixed`).
    pub const fn as_str(self) -> &'static str {
        match self {
            DurationSource::Exact => "exact",
            DurationSource::Cache => "cache",
            DurationSource::Estimate => "estimate",
            DurationSource::Source => "source",
            DurationSource::Fixed => "fixed",
        }
    }

    /// True for [`DurationSource::Estimate`].
    pub const fn is_estimate(self) -> bool {
        matches!(self, DurationSource::Estimate)
    }
}

impl std::fmt::Display for DurationSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the duration table passed to [`crate::layout`]; the table has one entry per
/// [`NodeId`] (index = node id).
///
/// What `frames` means depends on the node kind, always at the output sample rate:
/// - **cue**: length of the cue's audio after silence trimming;
/// - **clip**: length of the *whole* decoded source; the layout subtracts `trim_start_ms` itself;
/// - **pause**: ignored (the layout reads the pause length from the IR); use [`NodeDuration::fixed`]`(0)`.
///
/// For a clip, a `source` of [`DurationSource::Estimate`] means "the source length is not known" (for example
/// a missing asset during a dry run): the layout then honours requested clip lengths without cutting them to
/// the source and skips the `trim_start_ms` range check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeDuration {
    /// Length in frames at the output sample rate (see the type docs for per-kind meaning).
    pub frames: u64,
    /// Where the length came from.
    pub source: DurationSource,
}

impl NodeDuration {
    /// An entry with an explicit source.
    pub const fn new(frames: u64, source: DurationSource) -> Self {
        NodeDuration { frames, source }
    }

    /// Measured from audio synthesized in this run.
    pub const fn exact(frames: u64) -> Self {
        Self::new(frames, DurationSource::Exact)
    }

    /// Measured from cached audio.
    pub const fn cache(frames: u64) -> Self {
        Self::new(frames, DurationSource::Cache)
    }

    /// A heuristic estimate.
    pub const fn estimate(frames: u64) -> Self {
        Self::new(frames, DurationSource::Estimate)
    }

    /// Measured from a clip's source file (whole source, before `trim_start_ms`).
    pub const fn source(frames: u64) -> Self {
        Self::new(frames, DurationSource::Source)
    }

    /// Fixed by the document (used for pauses, whose entry the layout ignores).
    pub const fn fixed(frames: u64) -> Self {
        Self::new(frames, DurationSource::Fixed)
    }
}

/// A half-open interval of timeline frames, `[start, end)`. `end >= start` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Interval {
    /// First frame of the interval.
    pub start: i64,
    /// One past the last frame of the interval.
    pub end: i64,
}

impl Interval {
    /// `[start, end)`; an `end` before `start` is clamped to `start` (empty interval).
    pub fn new(start: i64, end: i64) -> Self {
        Interval {
            start,
            end: end.max(start),
        }
    }

    /// Number of frames covered.
    pub fn len(&self) -> u64 {
        self.end.abs_diff(self.start)
    }

    /// True when the interval covers no frame.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Number of frames shared with `other` (0 when they are disjoint or merely touch).
    pub fn overlap(&self, other: &Interval) -> u64 {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if end > start { end.abs_diff(start) } else { 0 }
    }

    /// True when the intervals share at least one frame.
    pub fn intersects(&self, other: &Interval) -> bool {
        self.overlap(other) > 0
    }

    /// True when `frame` lies inside `[start, end)`.
    pub fn contains(&self, frame: i64) -> bool {
        self.start <= frame && frame < self.end
    }
}

/// Sort intervals and merge the ones that overlap or touch. Empty intervals are dropped.
pub fn merge_intervals<I>(intervals: I) -> Vec<Interval>
where
    I: IntoIterator<Item = Interval>,
{
    let mut all: Vec<Interval> = intervals.into_iter().filter(|iv| !iv.is_empty()).collect();
    all.sort_unstable();
    let mut merged: Vec<Interval> = Vec::with_capacity(all.len());
    for iv in all {
        match merged.last_mut() {
            Some(last) if iv.start <= last.end => last.end = last.end.max(iv.end),
            _ => merged.push(iv),
        }
    }
    merged
}

/// What kind of node a [`Placement`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementKind {
    /// A speech cue.
    Cue,
    /// A pause in a speech sequence.
    Pause,
    /// An audio clip on a music or sfx track.
    Clip,
}

/// Where and how one node plays. All lengths and positions are frames at [`Layout::sample_rate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    /// The node this placement belongs to (index into `Program::nodes`).
    pub node: NodeId,
    /// The node's track.
    pub track: TrackIdx,
    /// Cue, pause or clip.
    pub kind: PlacementKind,
    /// First timeline frame (never negative: negative starts are clamped with `negative_start`).
    pub start: i64,
    /// Number of frames the node plays. For open-ended clips this is `total − start` (0 when the clip starts at
    /// or after the timeline end).
    pub length: u64,
    /// The clip loops until the timeline end (`loop: true` without a length).
    pub open_ended: bool,
    /// The clip loops its source (open-ended or not).
    pub looped: bool,
    /// The cue's tentative start before the overlap policy pushed it later (`cue_pushed`).
    pub pushed_from: Option<i64>,
    /// Where the length came from (pauses are `fixed`; clips are `fixed` unless their length is the source's
    /// natural length or was cut short by it, in which case the duration table's source is reported).
    pub duration_source: DurationSource,
    /// Clips: offset into the source where playback starts (`trim_start_ms` in frames). 0 otherwise.
    pub trim_start: u64,
    /// Clips: effective fade-in length after the crossfade adjustment (`max(fade_in_ms, crossfade_ms)`) and
    /// proportional scaling (`fades_scaled`). 0 otherwise.
    pub fade_in: u64,
    /// Clips: effective fade-out length, ending at the clip's end (for open-ended clips: at the timeline end).
    pub fade_out: u64,
    /// Curve of the fade-in: equal power when the fade comes from a crossfade, else the clip's `fade_curve`.
    pub fade_in_curve: FadeCurve,
    /// Curve of the fade-out: equal power when the fade comes from a crossfade, else the clip's `fade_curve`.
    pub fade_out_curve: FadeCurve,
    /// Looping clips: equal-power crossfade at each loop seam, at most half the looped segment. 0 otherwise.
    pub loop_crossfade: u64,
}

impl Placement {
    /// One past the last frame (`start + length`).
    pub fn end(&self) -> i64 {
        self.start
            .saturating_add(i64::try_from(self.length).unwrap_or(i64::MAX))
    }

    /// `[start, end)`.
    pub fn interval(&self) -> Interval {
        Interval::new(self.start, self.end())
    }
}

/// The span of a track: `[start of its first item, end of its last item]` in declaration order
/// (DESIGN.md §2.13), which is also what anchors that reference the track id resolve against: `with_cue` and
/// `before_cue` use the start of the first declared item, `after_cue` the end of the last declared item. When
/// another item of the track starts earlier or ends later, the layout reports `track_anchor_out_of_order` on
/// the anchors that are affected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackSpan {
    /// The track.
    pub track: TrackIdx,
    /// Start of the track's first declared item (or of the track itself when it has no items).
    pub start: i64,
    /// End of the track's last declared item (the timeline end when that item is open-ended).
    pub end: i64,
    /// The track's last declared item is an open-ended clip.
    pub open_ended: bool,
}

/// The result of [`crate::layout`]: sample-accurate geometry of every node plus layout diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// Output sample rate all frame values refer to.
    pub sample_rate: u32,
    /// Length of the output: `output.duration_ms` if set, else `content_end + tail`. Capped at
    /// [`soniccraft_core::MAX_TIME_MS`] (24 h) in frames; a longer timeline gets the error
    /// `timeline_too_long`, which names the uncapped length.
    pub total_frames: u64,
    /// Maximum end over all finite nodes (cues, pauses, clips that are not open-ended).
    pub content_end_frames: u64,
    /// The tail used after the content end (`output.tail_ms`, or the longest fade-out of open-ended clips).
    /// Informational when `output.duration_ms` forces the total.
    pub tail_frames: u64,
    /// Scheduling passes run by the `push` overlap policy (1..=[`crate::MAX_SCHEDULING_PASSES`]). The layout
    /// keeps the best pass (fewest and shortest remaining speech overlaps, then the least total push
    /// displacement, then the earliest), which is not necessarily the last one.
    pub passes: u32,
    /// One placement per node, indexed by [`NodeId`].
    pub placements: Vec<Placement>,
    /// One span per track, indexed by [`TrackIdx`].
    pub track_spans: Vec<TrackSpan>,
    /// Layout diagnostics (`speech_overlap`, `cue_pushed`, `negative_start`, `content_truncated`,
    /// `clip_shorter_than_requested`, `fades_scaled`, `no_fade_out_on_bed`, `track_anchor_out_of_order`,
    /// `timeline_too_long`, `field_not_applicable`, `invalid_value`, and defensive reference errors).
    pub diagnostics: Vec<Diagnostic>,
    /// Parallel to [`Layout::diagnostics`] (same length, same order): `true` when the diagnostic's condition
    /// involves a time that is derived from a duration-table entry whose source is
    /// [`DurationSource::Estimate`] (an estimated cue, or a clip whose source length is unknown), directly or
    /// through anchors, sequencing, clip lengths and the total. Under the `push` policy the outcome of a push
    /// depends on every speech interval already scheduled, so a pushed (or not pushed) implicit cue among
    /// estimate-dependent speech is estimate-dependent too, and so is every implicit cue when exact durations
    /// could change whether the policy re-runs (implicit and explicit speech with some estimate-dependent
    /// interval).
    ///
    /// The flags over-approximate: a `false` diagnostic holds whatever the estimated lengths turn out to be
    /// (static rule problems, conditions on exact or cached lengths and fixed times), so a render can stop on
    /// it before synthesizing anything; a `true` one may disappear or change with exact durations. The vector
    /// is empty in layouts deserialized from data written before it existed. See
    /// [`Layout::has_certain_errors`].
    #[serde(default)]
    pub estimate_dependent: Vec<bool>,
}

impl Layout {
    /// The placement of `node`, if it exists.
    pub fn placement(&self, node: NodeId) -> Option<&Placement> {
        self.placements.get(node)
    }

    /// The span of `track`, if it exists.
    pub fn track_span(&self, track: TrackIdx) -> Option<&TrackSpan> {
        self.track_spans.get(track)
    }

    /// Convert frames at [`Layout::sample_rate`] to milliseconds (0 when the sample rate is 0).
    pub fn ms(&self, frames: i64) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            frames_to_ms(frames, self.sample_rate)
        }
    }

    /// Start of `node` in milliseconds.
    pub fn start_ms(&self, node: NodeId) -> Option<f64> {
        self.placement(node).map(|p| self.ms(p.start))
    }

    /// End of `node` in milliseconds.
    pub fn end_ms(&self, node: NodeId) -> Option<f64> {
        self.placement(node).map(|p| self.ms(p.end()))
    }

    /// Length of `node` in milliseconds.
    pub fn duration_ms(&self, node: NodeId) -> Option<f64> {
        self.placement(node)
            .map(|p| self.ms(p.end()) - self.ms(p.start))
    }

    /// Original tentative start in milliseconds of a cue the overlap policy pushed later.
    pub fn pushed_from_ms(&self, node: NodeId) -> Option<f64> {
        self.placement(node)
            .and_then(|p| p.pushed_from)
            .map(|f| self.ms(f))
    }

    /// Total output length in milliseconds.
    pub fn total_ms(&self) -> f64 {
        self.ms(i64::try_from(self.total_frames).unwrap_or(i64::MAX))
    }

    /// Content end in milliseconds.
    pub fn content_end_ms(&self) -> f64 {
        self.ms(i64::try_from(self.content_end_frames).unwrap_or(i64::MAX))
    }

    /// Intervals of a track's audible items (cues and clips; pauses and empty items excluded), sorted by start.
    /// Suitable as cue-mode ducking triggers.
    pub fn track_intervals(&self, track: TrackIdx) -> Vec<Interval> {
        let mut out: Vec<Interval> = self
            .placements
            .iter()
            .filter(|p| p.track == track && p.kind != PlacementKind::Pause)
            .map(Placement::interval)
            .filter(|iv| !iv.is_empty())
            .collect();
        out.sort_unstable();
        out
    }

    /// Every speech cue with its interval, sorted by start (then node id). Empty cues are included.
    pub fn speech_cue_intervals(&self) -> Vec<(NodeId, Interval)> {
        let mut out: Vec<(NodeId, Interval)> = self
            .placements
            .iter()
            .filter(|p| p.kind == PlacementKind::Cue)
            .map(|p| (p.node, p.interval()))
            .collect();
        out.sort_unstable_by_key(|&(node, iv)| (iv.start, iv.end, node));
        out
    }

    /// True when at least one cue or clip length is an estimate.
    pub fn estimated(&self) -> bool {
        self.placements
            .iter()
            .any(|p| p.duration_source.is_estimate())
    }

    /// Number of cue and clip lengths that are estimates.
    pub fn estimated_count(&self) -> usize {
        self.placements
            .iter()
            .filter(|p| p.duration_source.is_estimate())
            .count()
    }

    /// True when the layout reported an error (an `error` overlap policy violation or a defensive reference
    /// error); the engine must not render such a layout.
    pub fn has_errors(&self) -> bool {
        diagnostics::has_errors(&self.diagnostics)
    }

    /// True when diagnostic `index` depends on estimated durations (see [`Layout::estimate_dependent`]);
    /// false for an index out of range.
    pub fn depends_on_estimates(&self, index: usize) -> bool {
        self.estimate_dependent.get(index).copied().unwrap_or(false)
    }

    /// Every diagnostic with its [`Layout::estimate_dependent`] flag.
    pub fn diagnostics_with_estimates(&self) -> impl Iterator<Item = (&Diagnostic, bool)> + '_ {
        self.diagnostics
            .iter()
            .enumerate()
            .map(|(i, d)| (d, self.depends_on_estimates(i)))
    }

    /// True when the layout reported an error that does not depend on estimated durations: exact durations
    /// cannot make it go away, so a render can stop before synthesizing anything.
    pub fn has_certain_errors(&self) -> bool {
        self.diagnostics_with_estimates()
            .any(|(d, estimated)| d.is_error() && !estimated)
    }
}
