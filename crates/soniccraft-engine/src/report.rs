//! Serializable results of engine operations. These shapes are a public contract: the CLI prints them
//! with `--json`, the MCP server returns them as `structuredContent`, the manifest file and the web
//! preview consume them. See docs/DESIGN.md §8. Add fields freely; never rename or remove them.
//!
//! Times are milliseconds from the start of the output (`f64`, exact frame conversions at the output
//! sample rate). Paths are absolute in API results and relative to the file's own directory in the
//! manifest and the preview payload.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use soniccraft_core::Diagnostic;

/// Result of `validate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// True when there are no error diagnostics.
    pub valid: bool,
    /// Every problem and note, sorted by document position.
    pub diagnostics: Vec<Diagnostic>,
    /// Present when the timeline could be lowered far enough to summarize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<TimelineSummary>,
}

/// High-level facts about a timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineSummary {
    /// Number of tracks.
    pub tracks: usize,
    /// Number of speech cues.
    pub speech_cues: usize,
    /// Number of audio clips.
    pub clips: usize,
    /// Distinct speaker names used by cues, in order of first use.
    pub speakers: Vec<String>,
    /// Provider instance names used by cues.
    pub providers: Vec<String>,
    /// Total characters of cue text.
    pub characters: usize,
    /// Cues whose audio is already in the TTS cache.
    pub cached_cues: usize,
    /// Output format (`wav`, `mp3`, ...).
    pub output_format: String,
    /// Output sample rate in Hz.
    pub sample_rate: u32,
    /// Master loudness target in LUFS.
    pub target_lufs: f64,
}

/// Result of `plan` (dry run): layout with cached, mock (exact) or estimated durations, no paid API calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanReport {
    /// True when there are no error diagnostics (layout included).
    pub valid: bool,
    /// Validation and layout diagnostics, sorted by document position. Layout errors that depend on
    /// estimated durations are warnings here, their message ending in "(based on estimated speech
    /// durations; the render checks again with exact durations)"; a render over the memory budget is a
    /// `timeline_too_long` warning.
    pub diagnostics: Vec<Diagnostic>,
    /// Total length of the output.
    pub duration_ms: f64,
    /// True when at least one duration is an estimate.
    pub estimated: bool,
    /// Output sample rate in Hz.
    pub sample_rate: u32,
    /// Tracks in declaration order.
    pub tracks: Vec<TrackReport>,
    /// Every cue, pause and clip, sorted by start time (ties: declaration order).
    pub items: Vec<ItemReport>,
    /// Ducking activity of every ducked track, computed from the planned item intervals.
    pub ducking: Vec<DuckingReport>,
    /// What a render would synthesize.
    pub synthesis: SynthesisPlan,
    /// ASCII timeline diagram (cut after 256 KB for very large timelines, with a note; `items` has every
    /// item).
    pub diagram: String,
}

/// One track in a plan/render report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackReport {
    /// Track id.
    pub id: String,
    /// speech | music | sfx
    #[serde(rename = "type")]
    pub kind: String,
    /// Track fader in dB (`volume` and `gain_db` combined; -120 or less means silent).
    pub gain_db: f64,
    /// Stereo position in [-1, 1].
    pub pan: f64,
    /// The track is laid out and synthesized but not mixed.
    pub mute: bool,
    /// Ids of tracks that duck this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ducked_by: Vec<String>,
}

/// Item kind in reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A speech cue.
    Cue,
    /// A pause in a speech sequence.
    Pause,
    /// An audio clip.
    Clip,
}

/// Where an item's duration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationSource {
    /// Measured from audio synthesized in this run; in plans, from the `mock` voice rendered in-process
    /// (exactly what a render produces). The manifest file uses it for cached audio too.
    Exact,
    /// Measured from cached audio.
    Cache,
    /// Heuristic estimate (dry run).
    Estimate,
    /// Measured from the clip's source file.
    Source,
    /// Fixed by the document (pauses, explicit clip lengths).
    Fixed,
}

/// One placed item (cue, pause or clip).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemReport {
    /// Item id (explicit, or generated such as `voice.cue2` / `voice.pause1`).
    pub id: String,
    /// Cue, pause or clip.
    pub kind: ItemKind,
    /// Id of the item's track.
    pub track_id: String,
    /// Start of the item.
    pub start_ms: f64,
    /// End of the item.
    pub end_ms: f64,
    /// `end_ms - start_ms`.
    pub duration_ms: f64,
    /// Where the duration came from.
    pub duration_source: DurationSource,
    /// Speaker name (cues with a speaker).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    /// Provider instance (cues).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Voice id (cues).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_id: Option<String>,
    /// Cue text (cues) — full text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Source path as written (clips).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Original tentative start when the overlap policy moved this cue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_from_ms: Option<f64>,
    /// Clip runs until the timeline end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_ended: bool,
    /// Clip loops its source.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub looped: bool,
}

/// Ducking activity for one ducked track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuckingReport {
    /// The ducked track.
    pub track_id: String,
    /// The configured mode: sidechain | cue. Dry runs compute the regions from item intervals (as `cue`
    /// does) whatever the mode.
    pub mode: String,
    /// Positive attenuation depth in dB.
    pub reduction_db: f64,
    /// Trigger tracks that drive the ducking (muted triggers are excluded).
    pub triggers: Vec<String>,
    /// Active regions after hold and lookahead.
    pub regions: Vec<Region>,
}

/// A half-open time interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Region {
    /// Start of the interval.
    pub start_ms: f64,
    /// End of the interval (exclusive).
    pub end_ms: f64,
}

/// What a render would synthesize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynthesisPlan {
    /// Speech cues in the timeline.
    pub cues: usize,
    /// Cues already in the TTS cache.
    pub cached: usize,
    /// Characters a render would send to providers (identical requests count once).
    pub characters_to_synthesize: usize,
    /// Per provider instance, in order of first use.
    pub by_provider: IndexMap<String, ProviderSynthesisPlan>,
}

/// Per provider instance synthesis plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSynthesisPlan {
    /// Built-in provider type of the instance.
    pub provider_type: String,
    /// Cues using the instance.
    pub cues: usize,
    /// Of those, cues already in the TTS cache.
    pub cached: usize,
    /// Characters a render would send to the instance (identical requests count once).
    pub characters: usize,
}

/// Result of a successful `render`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RenderReport {
    /// The main output file.
    pub output: OutputReport,
    /// Loudness of the delivered file (see [`LoudnessReport`]).
    pub loudness: LoudnessReport,
    /// Side files.
    pub files: FilesReport,
    /// Tracks in declaration order.
    pub tracks: Vec<TrackReport>,
    /// Every cue, pause and clip with exact times, sorted by start time (ties: declaration order).
    pub items: Vec<ItemReport>,
    /// Ducking activity of every ducked track.
    pub ducking: Vec<DuckingReport>,
    /// What was synthesized. Run provenance: omitted from the manifest file.
    #[serde(default)]
    pub synthesis: SynthesisReport,
    /// Every problem and note, sorted by document position.
    pub diagnostics: Vec<Diagnostic>,
    /// Wall-clock stage timings. Not deterministic: omitted from the manifest file.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub timings_ms: IndexMap<String, f64>,
}

/// The main output file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputReport {
    /// Absolute path in API results; relative to the manifest in the manifest file.
    pub path: String,
    /// Container/codec (`wav`, `mp3`, `flac`, `ogg`, `opus`, `m4a`).
    pub format: String,
    /// Sample rate of the file in Hz (48000 for opus).
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Bits per sample (wav, flac).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u16>,
    /// Bitrate actually used (lossy formats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u32>,
    /// Length of the output.
    pub duration_ms: f64,
    /// File size.
    pub bytes: u64,
}

/// Loudness of the delivered output.
///
/// `integrated_lufs`, `true_peak_dbtp`, `sample_peak_dbfs` and `loudness_range_lu` describe the file that
/// was delivered. For wav and flac that is the mastered signal itself. For the lossy formats (mp3, ogg,
/// opus, m4a) the encoded file is decoded and measured with the same BS.1770 meter: codecs reshape peaks
/// and shift the loudness slightly, so these are the values a player sees. When the encoded file peaks
/// above `true_peak_target_dbtp`, the master is limited again with the ceiling lowered by the overshoot
/// plus 0.1 dB; when its loudness is more than 0.2 LU off a target the master had reached, the difference
/// goes back into the master as gain. Either way it is encoded again (at most 3 extra passes,
/// `pre_encode.compensation_passes`); a file still above the ceiling gets the warning `true_peak_exceeded`,
/// one still off the target `loudness_target_missed`. `pre_encode` then holds the values of the
/// master that was encoded. `gain_db`, `limiter_max_reduction_db` and `iterations` always describe the
/// mastering loop.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoudnessReport {
    /// Integrated loudness of the delivered file (None for silent or too-short output).
    pub integrated_lufs: Option<f64>,
    /// True peak of the delivered file (None for silent output).
    pub true_peak_dbtp: Option<f64>,
    /// Sample peak of the delivered file (None for silent output).
    pub sample_peak_dbfs: Option<f64>,
    /// Loudness range (EBU Tech 3342) of the delivered file, when defined.
    pub loudness_range_lu: Option<f64>,
    /// The integrated loudness target.
    pub target_lufs: f64,
    /// The true-peak ceiling.
    pub true_peak_target_dbtp: f64,
    /// Whether loudness normalization ran.
    pub normalized: bool,
    /// Gain applied by loudness normalization.
    pub gain_db: f64,
    /// Maximum gain reduction applied by the true-peak limiter.
    pub limiter_max_reduction_db: f64,
    /// Gain → limit → measure passes of the mastering loop.
    pub iterations: u32,
    /// Lossy formats only: the master as it went into the encoder (None for wav and flac, whose files
    /// carry the master itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_encode: Option<PreEncodeLoudness>,
}

/// The master that a lossy encoder received (see [`LoudnessReport`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PreEncodeLoudness {
    /// Integrated loudness of the encoded master.
    pub integrated_lufs: Option<f64>,
    /// True peak of the encoded master.
    pub true_peak_dbtp: Option<f64>,
    /// Sample peak of the encoded master.
    pub sample_peak_dbfs: Option<f64>,
    /// Loudness range of the encoded master, when defined.
    pub loudness_range_lu: Option<f64>,
    /// The limiter ceiling (dBTP) of the encoded master: `true_peak_target_dbtp`, lowered once per
    /// compensation pass.
    pub ceiling_dbtp: f64,
    /// Times the master was limited and encoded again because the encoded file peaked above the
    /// ceiling (0–2).
    pub compensation_passes: u32,
}

/// Side files written by a render.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FilesReport {
    /// Subtitle exports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<FileRef>,
    /// Per-track stems.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stems: Vec<StemRef>,
    /// The manifest (`<stem>.manifest.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The web preview (`<stem>.preview.html`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// A subtitle file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRef {
    /// vtt | srt | json | transcript
    pub format: String,
    /// Where the file is.
    pub path: String,
}

/// A stem file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StemRef {
    /// The stem's track.
    pub track_id: String,
    /// Where the file is (32-bit float WAV).
    pub path: String,
}

/// What a render synthesized.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SynthesisReport {
    /// Speech cues in the timeline.
    pub cues: usize,
    /// Cues served from the TTS cache.
    pub cache_hits: usize,
    /// Provider requests made (cues with identical text and voice are synthesized once).
    pub synthesized: usize,
    /// Provider instances that were actually called (not served from cache).
    pub providers_called: Vec<String>,
}

/// A failed render: the timeline was invalid or a stage failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderFailure {
    /// What went wrong, in one sentence.
    pub message: String,
    /// The diagnostics that explain the failure (errors first by position), plus warnings and notes.
    pub diagnostics: Vec<Diagnostic>,
    /// True when the render stopped because its [`crate::CancelFlag`] was set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

impl std::fmt::Display for RenderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RenderFailure {}

/// Built-in providers and their status in this environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvidersReport {
    /// Every built-in provider type, in display order.
    pub providers: Vec<ProviderStatus>,
}

/// Status of one built-in provider type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// Canonical type name, e.g. "elevenlabs".
    pub name: String,
    /// Other accepted spellings, e.g. "11labs".
    pub aliases: Vec<String>,
    /// Human-readable name, e.g. "ElevenLabs".
    pub display_name: String,
    /// Runs locally (no network/API cost).
    pub local: bool,
    /// Returns word timings (otherwise they are estimated).
    pub word_timestamps: bool,
    /// Whether it can be used right now: credentials present, binary found, platform supported, its local
    /// server answering (Kokoro-FastAPI and other servers on this machine or a private network are
    /// probed), and allowed by the engine (`command` needs `allow_command`).
    pub available: bool,
    /// Why not, or how it is configured.
    pub status: String,
    /// Env var holding the API key, if one is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Voice used when a cue sets no `voice_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_voice: Option<String>,
    /// Model used when a cue sets no `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Short usage notes.
    pub notes: String,
}

/// Voices offered by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoicesReport {
    /// Canonical provider type name.
    pub provider: String,
    /// The voices, in the provider's order.
    pub voices: Vec<VoiceInfo>,
}

/// One voice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceInfo {
    /// The value to put in `voice_id`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Language or locale, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Gender label, when the provider publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    /// Free-form description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// TTS cache statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheInfo {
    /// The cache directory (absolute).
    pub path: String,
    /// Complete entries (audio + metadata).
    pub entries: usize,
    /// Bytes used by cache files.
    pub bytes: u64,
}
