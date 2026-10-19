//! The lowered, validated intermediate representation of a timeline.
//!
//! [`crate::lower::lower`] turns a raw [`crate::model::Timeline`] into a [`Program`]: every default is
//! resolved, every enum parsed, every duration converted to milliseconds, every reference resolved to an
//! index, and every speech cue has a fully merged voice. Downstream crates (layout, TTS, engine) only ever
//! see this IR, never the raw AST.
//!
//! Times in the IR are milliseconds (`f64`); the layout engine converts them to frames at
//! [`OutputConfig::sample_rate`].

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value};

pub use crate::model::{
    DuckingModeName as DuckingMode, FadeCurveName as FadeCurve,
    LoudnessPresetName as LoudnessPreset, OutputFormatName as OutputFormat,
    OverlapPolicyName as OverlapPolicy, ProviderTypeName as ProviderType,
    SubtitleFormatName as SubtitleFormat, TrackKindName as TrackKind,
};

/// Index into [`Program::nodes`].
pub type NodeId = usize;
/// Index into [`Program::tracks`].
pub type TrackIdx = usize;

/// A fully resolved timeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Program {
    /// Normalized DSL version, e.g. "1.0".
    pub version: String,
    /// Optional human-readable title (metadata).
    pub title: Option<String>,
    /// Directory relative paths are resolved against (the timeline's directory).
    pub base_dir: PathBuf,
    /// Output file, format and mastering settings.
    pub output: OutputConfig,
    /// Caption layout settings.
    pub subtitles: SubtitleConfig,
    /// Timing and speech-overlap policy.
    pub timing: TimingConfig,
    /// Mix-wide settings.
    pub mix: MixConfig,
    /// Tracks in document order (index = [`TrackIdx`]).
    pub tracks: Vec<TrackIr>,
    /// Every cue, pause and clip, in document order (track by track, item by item).
    pub nodes: Vec<Node>,
    /// Provider instances: every configured instance plus every built-in referenced by a cue.
    pub providers: IndexMap<String, ProviderInstance>,
    /// Every id (tracks, cues, clips) → what it refers to.
    pub ids: IndexMap<String, AnchorTarget>,
}

impl Program {
    /// Iterate over all speech cues with their node ids.
    pub fn cues(&self) -> impl Iterator<Item = (NodeId, &Node, &CueIr)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| match &n.kind {
                NodeKind::Cue(c) => Some((i, n, c)),
                _ => None,
            })
    }

    /// Iterate over all audio clips with their node ids.
    pub fn clips(&self) -> impl Iterator<Item = (NodeId, &Node, &ClipIr)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| match &n.kind {
                NodeKind::Clip(c) => Some((i, n, c)),
                _ => None,
            })
    }

    /// Look up a track by id.
    pub fn track_index(&self, id: &str) -> Option<TrackIdx> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// Convert milliseconds to frames at the output sample rate (rounded to nearest, saturating; see
    /// [`ms_to_frames`]).
    pub fn frames(&self, ms: f64) -> i64 {
        ms_to_frames(ms, self.output.sample_rate)
    }

    /// Convert milliseconds to frames at the output sample rate; None when the result is not a finite
    /// number of frames that fits in an `i64` (see [`checked_ms_to_frames`]).
    pub fn checked_frames(&self, ms: f64) -> Option<i64> {
        checked_ms_to_frames(ms, self.output.sample_rate)
    }
}

/// `round(ms × sample_rate / 1000)`, saturating: results beyond the `i64` range clamp to `i64::MIN` /
/// `i64::MAX` and NaN gives 0.
///
/// Every time in a lowered [`Program`] is finite and within ±[`crate::MAX_TIME_MS`], so its frame count is
/// far from the limits (24 h at 192 kHz is about 1.7e10 frames). For values computed from other inputs
/// (estimates, sums), use [`checked_ms_to_frames`] and report the overflow instead of computing with a
/// clamped value.
pub fn ms_to_frames(ms: f64, sample_rate: u32) -> i64 {
    (ms * sample_rate as f64 / 1000.0).round() as i64
}

/// `round(ms × sample_rate / 1000)`, or None when `ms` is not finite or the frame count does not fit in an
/// `i64`.
pub fn checked_ms_to_frames(ms: f64, sample_rate: u32) -> Option<i64> {
    let frames = (ms * sample_rate as f64 / 1000.0).round();
    // `i64::MAX as f64` rounds up to 2^63, which itself does not fit: the upper bound is exclusive.
    (frames.is_finite() && frames >= i64::MIN as f64 && frames < i64::MAX as f64)
        .then_some(frames as i64)
}

/// `frames × 1000 / sample_rate`.
pub fn frames_to_ms(frames: i64, sample_rate: u32) -> f64 {
    frames as f64 * 1000.0 / sample_rate as f64
}

/// What an id refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "index", rename_all = "snake_case")]
pub enum AnchorTarget {
    /// A cue or clip (pauses are never targets).
    Node(NodeId),
    /// A track; its span is [start of its first item, end of its last item].
    Track(TrackIdx),
}

/// Resolved output settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputConfig {
    /// Container/codec of the main output file.
    pub format: OutputFormat,
    /// True when the format came from `output.format` or an explicit override (not inferred).
    pub format_explicit: bool,
    /// Output path as written (relative to `Program::base_dir`), after CLI/MCP overrides.
    pub path: Option<String>,
    /// Sample rate of the mix in Hz (opus files are always encoded at 48000).
    pub sample_rate: u32,
    /// 1 (mono) or 2 (stereo).
    pub channels: u16,
    /// Bits per second for lossy formats (None for wav/flac).
    pub bitrate_bps: Option<u32>,
    /// 16, 24 or 32 (32 = float, wav only). Meaningless for lossy formats.
    pub bit_depth: u16,
    /// The preset `target_lufs` and `true_peak_db` default to.
    pub loudness_preset: LoudnessPreset,
    /// Integrated loudness target of the master in LUFS.
    pub target_lufs: f64,
    /// True-peak ceiling of the master in dBTP.
    pub true_peak_db: f64,
    /// Apply the master loudness gain; when false only the true-peak limiter runs (clip guard).
    pub normalize: bool,
    /// Requested subtitle formats (empty = no subtitle export).
    pub subtitle_formats: Vec<SubtitleFormat>,
    /// Write one 32-bit float WAV stem per track to `<stem>.stems/`.
    pub export_stems: bool,
    /// Write the render report to `<stem>.manifest.json`.
    pub export_manifest: bool,
    /// Explicit time after the last finite item; None = automatic (DESIGN.md §3.5).
    pub tail_ms: Option<f64>,
    /// Forced total duration; content beyond it is cut.
    pub duration_ms: Option<f64>,
    /// TPDF dither (fixed seed) when quantizing to 16-bit PCM.
    pub dither: bool,
}

/// Resolved caption layout settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SubtitleConfig {
    /// Hard line-length limit in characters.
    pub max_chars_per_line: usize,
    /// Lines per caption.
    pub max_lines: usize,
    /// Longest on-screen time of one caption.
    pub max_duration_ms: f64,
    /// Captions are extended toward this length when the next caption allows it.
    pub min_duration_ms: f64,
    /// Prefix captions with speaker labels (explicit setting, else on when the cues have 2+ distinct labels).
    pub speaker_labels: bool,
}

/// Resolved timing policy.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimingConfig {
    /// What happens when speech cues overlap (DESIGN.md §3.4).
    pub speech_overlap: OverlapPolicy,
    /// Gap used when `push` moves a cue that has no preceding pause.
    pub min_gap_ms: f64,
    /// Speech overlaps up to this long are ignored.
    pub overlap_tolerance_ms: f64,
}

/// Resolved mix settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MixConfig {
    /// Source-normalization target of tracks with `normalize` enabled and no explicit target.
    pub reference_lufs: f64,
}

/// A resolved track.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrackIr {
    /// Track id.
    pub id: String,
    /// Track type.
    pub kind: TrackKind,
    /// Diagnostic path, e.g. `tracks[3]`.
    pub path: String,
    /// Linear fader gain: `volume × 10^(gain_db/20)`.
    pub gain: f64,
    /// Stereo position in [-1, 1] (constant power for mono sources, balance for stereo).
    pub pan: f64,
    /// Excluded from the mix (still laid out and synthesized).
    pub mute: bool,
    /// Source-level loudness target in LUFS, or None when normalization is off for this track.
    pub normalize_lufs: Option<f64>,
    /// Silence trimming for speech cues (None = off, or not a speech track).
    pub trim_silence: Option<TrimConfig>,
    /// Ducking of this track by its trigger tracks, if configured.
    pub ducking: Option<DuckingConfig>,
    /// Where the track's first sequential item starts. [`StartRule::Sequential`] means the document sets no
    /// track start (the track starts at 0); `start_at_ms` gives [`StartRule::Absolute`] and `start_relative`
    /// gives [`StartRule::Relative`].
    pub start: StartRule,
    /// The track's nodes in order (pauses included).
    pub nodes: Vec<NodeId>,
}

/// Silence trimming parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrimConfig {
    /// Level in dBFS below which audio counts as silence.
    pub threshold_db: f64,
    /// Silence kept before the first and after the last audible frame.
    pub padding_ms: f64,
}

/// Resolved ducking parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DuckingConfig {
    /// Tracks whose activity ducks this one (deduplicated; never the track itself).
    pub triggers: Vec<TrackIdx>,
    /// Positive attenuation depth in dB (e.g. 18.0 means -18 dB).
    pub reduction_db: f64,
    /// Ramp time from 0 dB to full depth.
    pub attack_ms: f64,
    /// Ramp time from full depth back to 0 dB.
    pub release_ms: f64,
    /// Activity gaps shorter than this keep the duck engaged; activity is held this long.
    pub hold_ms: f64,
    /// Activity is detected this much earlier (defaults to `attack_ms`).
    pub lookahead_ms: f64,
    /// RMS threshold in dBFS for `sidechain` mode.
    pub threshold_db: f64,
    /// Signal-driven (`sidechain`) or timeline-driven (`cue`) activity detection.
    pub mode: DuckingMode,
}

/// A schedulable item: a speech cue, a pause, or an audio clip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    /// Explicit or generated id (`<track>.cue<n>`, `<track>.clip<n>`, `<track>.pause<n>`); the single-clip
    /// shorthand clip uses the track id.
    pub id: String,
    /// True when the id was generated.
    pub auto_id: bool,
    /// The track this node belongs to.
    pub track: TrackIdx,
    /// Position within the track's item list.
    pub index_in_track: usize,
    /// Diagnostic path, e.g. `tracks[1].sequence[2]` or `tracks[0]` (shorthand clip).
    pub path: String,
    /// How the start is determined.
    pub start: StartRule,
    /// Cue, pause or clip payload.
    pub kind: NodeKind,
}

impl Node {
    /// True for speech cues.
    pub fn is_cue(&self) -> bool {
        matches!(self.kind, NodeKind::Cue(_))
    }

    /// The cue payload, if this node is a cue.
    pub fn as_cue(&self) -> Option<&CueIr> {
        match &self.kind {
            NodeKind::Cue(c) => Some(c),
            _ => None,
        }
    }

    /// The clip payload, if this node is a clip.
    pub fn as_clip(&self) -> Option<&ClipIr> {
        match &self.kind {
            NodeKind::Clip(c) => Some(c),
            _ => None,
        }
    }
}

/// What a node is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeKind {
    /// A speech cue.
    Cue(CueIr),
    /// A pause of a fixed length.
    Pause {
        /// Pause length in milliseconds.
        duration_ms: f64,
    },
    /// An audio clip.
    Clip(ClipIr),
}

/// How a node's (or a track's) start is determined.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum StartRule {
    /// After the previous item of the same track (or at the track start for the first item).
    Sequential,
    /// At an absolute time.
    Absolute {
        /// Start time in milliseconds.
        ms: f64,
    },
    /// Relative to another item or track.
    Relative(AnchorRule),
}

impl StartRule {
    /// True for absolute and relative starts (scheduled before sequential ones).
    pub fn is_explicit(&self) -> bool {
        !matches!(self, StartRule::Sequential)
    }
}

/// A resolved anchor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnchorRule {
    /// The resolved target.
    pub target: AnchorTarget,
    /// The id as written (for messages).
    pub target_id: String,
    /// Which edge of the target is used.
    pub mode: AnchorMode,
    /// Signed offset added to the anchor time.
    pub offset_ms: f64,
}

/// Which edge of the anchor target is used, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorMode {
    /// `after_cue`: time = end(target) + offset.
    After,
    /// `with_cue`: time = start(target) + offset.
    With,
    /// `before_cue` (starts only): this node ends at start(target) + offset.
    Before,
}

/// A resolved speech cue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CueIr {
    /// Text sent to the TTS provider (trimmed).
    pub text: String,
    /// Subtitle text override.
    pub caption: Option<String>,
    /// Include in subtitle exports.
    pub subtitles: bool,
    /// Effective speaker key: the cue's `speaker`, else the track's (None when neither is set).
    pub speaker: Option<String>,
    /// Display label for subtitles: speaker label, else speaker key, else track id.
    pub speaker_label: String,
    /// The fully merged voice.
    pub voice: VoiceIr,
    /// Linear per-cue gain: cue volume × 10^((cue gain_db + speaker gain_db)/20).
    pub gain: f64,
    /// Exempt from speech-overlap handling (intentional crosstalk).
    pub allow_overlap: bool,
}

impl CueIr {
    /// The text shown in subtitles.
    pub fn display_text(&self) -> &str {
        self.caption.as_deref().unwrap_or(&self.text)
    }
}

/// A fully merged voice for one cue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VoiceIr {
    /// Provider instance name (key of [`Program::providers`]).
    pub provider: String,
    /// Built-in type of the provider instance.
    pub provider_type: ProviderType,
    /// Provider voice id (the provider's default voice when none was set).
    pub voice_id: Option<String>,
    /// Model id (the instance's model or the provider default when none was set).
    pub model: Option<String>,
    /// Language code, if any layer set one.
    pub language: Option<String>,
    /// Merged provider-specific settings.
    pub settings: Map<String, Value>,
}

/// A resolved provider instance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderInstance {
    /// Instance name (key of [`Program::providers`]).
    pub name: String,
    /// Built-in provider type.
    pub provider_type: ProviderType,
    /// True when the instance appears under `providers:` in the timeline.
    pub configured: bool,
    /// Env var holding the API key (None = the provider needs no key).
    pub api_key_env: Option<String>,
    /// API base URL (configured, else the type's base-URL env var, else the default; None for local processes).
    pub base_url: Option<String>,
    /// Default model of this instance (configured, else the provider default).
    pub model: Option<String>,
    /// Provider-native output format override.
    pub output_format: Option<String>,
    /// Parallel requests to this instance.
    pub max_concurrency: usize,
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Configured speaking rate for estimates (characters per second), if any.
    pub chars_per_second: Option<f64>,
    /// Configured speaking rate for estimates (words per minute), if any.
    pub words_per_minute: Option<f64>,
    /// Default voice settings of this instance (lowest-priority settings layer).
    pub settings: Map<String, Value>,
    /// argv template (`command` / `piper`).
    pub command: Option<Vec<String>>,
    /// Template written to the child's stdin (`command` / `piper`).
    pub stdin: Option<String>,
    /// Directory searched for `<voice_id>.onnx` (`piper`).
    pub voices_dir: Option<String>,
}

/// A resolved audio clip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClipIr {
    /// Source path as written.
    pub source: String,
    /// `base_dir.join(source)`.
    pub source_path: PathBuf,
    /// Offset into the source where playback begins.
    pub trim_start_ms: f64,
    /// How the clip length is determined.
    pub length: ClipLength,
    /// Loop the source (from `trim_start_ms`) to fill the clip length.
    pub looped: bool,
    /// Equal-power crossfade at each loop seam.
    pub loop_crossfade_ms: f64,
    /// Fade-in length at the clip start.
    pub fade_in_ms: f64,
    /// Fade-out length at the clip end.
    pub fade_out_ms: f64,
    /// Fade shape.
    pub fade_curve: FadeCurve,
    /// Overlap with the previous clip of the same track.
    pub crossfade_ms: f64,
    /// Linear per-clip gain.
    pub gain: f64,
    /// True for the track-level single-clip shorthand.
    pub shorthand: bool,
}

/// How a clip's length is determined (see DESIGN.md §3.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum ClipLength {
    /// The source's natural length (non-looping) or open-ended (looping).
    Natural,
    /// An explicit play length (`duration_ms`).
    Duration {
        /// Requested length in milliseconds.
        ms: f64,
    },
    /// Ends at an absolute time (`end_at_ms`).
    EndAbsolute {
        /// End time in milliseconds.
        ms: f64,
    },
    /// Ends relative to another item or track (`end_relative`).
    EndRelative(AnchorRule),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_conversions_saturate_or_report_overflow() {
        assert_eq!(ms_to_frames(1000.0, 48_000), 48_000);
        assert_eq!(
            ms_to_frames(0.5 / 48.0, 48_000),
            1,
            "rounds half away from zero"
        );
        assert_eq!(checked_ms_to_frames(1000.0, 48_000), Some(48_000));
        assert_eq!(checked_ms_to_frames(-250.0, 48_000), Some(-12_000));
        let day = crate::MAX_TIME_MS;
        assert_eq!(checked_ms_to_frames(day, 192_000), Some(16_588_800_000));
        // 1e18 ms at 48 kHz is 4.8e19 frames: beyond i64.
        assert_eq!(checked_ms_to_frames(1e18, 48_000), None);
        assert_eq!(checked_ms_to_frames(-1e18, 48_000), None);
        assert_eq!(checked_ms_to_frames(f64::NAN, 48_000), None);
        assert_eq!(checked_ms_to_frames(f64::INFINITY, 48_000), None);
        assert_eq!(ms_to_frames(1e18, 48_000), i64::MAX);
        assert_eq!(ms_to_frames(-1e18, 48_000), i64::MIN);
        assert_eq!(ms_to_frames(f64::NAN, 48_000), 0);
        assert_eq!(frames_to_ms(48_000, 48_000), 1000.0);
    }
}
