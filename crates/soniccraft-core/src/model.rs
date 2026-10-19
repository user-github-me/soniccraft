//! Raw timeline AST: the exact shape of `timeline.yaml` / `timeline.json` (DSL v1.0).
//!
//! These types are deliberately permissive. Every field is optional at the serde level and enum-like
//! values are plain strings, so a single typo never aborts parsing. All semantic rules (required fields,
//! which fields apply to which track type, ranges, enum values, references) are enforced by
//! [`crate::lower`], which reports *every* problem as a structured [`crate::diagnostics::Diagnostic`].
//! Unknown keys are collected during parsing (see [`crate::parse`]) and reported with did-you-mean hints.
//!
//! The JSON Schema shipped with SonicCraft is generated from these types (see [`crate::schema`]); doc
//! comments become schema descriptions, so keep them accurate and agent-readable.

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::values::{Bitrate, Millis, NormalizeInput, SubtitleExport, TrimSilenceInput, Version};

/// A SonicCraft timeline document (DSL v1.0).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct Timeline {
    /// DSL version. Must be "1.0" (also accepted: "1", 1, 1.0).
    pub version: Option<Version>,
    /// Optional human-readable title (metadata only).
    pub title: Option<String>,
    /// Optional description (metadata only).
    pub description: Option<String>,
    /// Output file, format and mastering settings.
    pub output: Option<OutputSpec>,
    /// Caption layout settings. Subtitle export itself is enabled with `output.export_subtitles`.
    pub subtitles: Option<SubtitleSpec>,
    /// Timing and overlap policy.
    pub timing: Option<TimingSpec>,
    /// Mix-wide settings.
    pub mix: Option<MixSpec>,
    /// Voice settings applied to every speech cue (lowest priority after provider defaults).
    pub defaults: Option<VoiceSpec>,
    /// Named speakers. A cue or speech track with `speaker: <name>` inherits that speaker's voice.
    #[serde(default)]
    pub speakers: IndexMap<String, VoiceSpec>,
    /// Named TTS provider instances. Cues reference them by name via `provider`.
    /// Built-in provider names (elevenlabs, openai, cartesia, kokoro/local_kokoro, piper, macos_say, command, mock)
    /// work without any entry here.
    #[serde(default)]
    pub providers: IndexMap<String, ProviderSpec>,
    /// The tracks of the composition. Required and non-empty.
    #[serde(default)]
    pub tracks: Vec<Track>,
}

/// Output file, format and mastering settings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct OutputSpec {
    /// Container/codec: wav, mp3, flac, ogg, opus or m4a. Default: from the output path extension, else wav.
    /// ogg, opus and m4a require ffmpeg on PATH.
    #[schemars(with = "Option<OutputFormatName>")]
    pub format: Option<String>,
    /// Output path relative to the timeline file. Default: `<timeline stem>.<ext>` next to the timeline.
    pub path: Option<String>,
    /// Output sample rate in Hz. Default 48000.
    pub sample_rate: Option<u32>,
    /// Output channel count: 1 (mono) or 2 (stereo). Default 2.
    pub channels: Option<u16>,
    /// Bitrate for lossy formats, e.g. "320k", "192k" or 192000. Default mp3 192k, ogg 160k, opus 96k, m4a 160k.
    pub bitrate: Option<Bitrate>,
    /// Bit depth for wav (16, 24, or 32 = float) and flac (16 or 24). Default 24.
    pub bit_depth: Option<u16>,
    /// Loudness preset: podcast (-16 LUFS, -1 dBTP), streaming (-14, -1), broadcast (EBU R128: -23, -1),
    /// atsc (A/85: -24, -2). Default podcast.
    #[schemars(with = "Option<LoudnessPresetName>")]
    pub loudness_preset: Option<String>,
    /// Integrated loudness target of the master in LUFS (overrides the preset). Range -60..-5.
    pub target_lufs: Option<f64>,
    /// Maximum true peak of the master in dBTP (overrides the preset). Range -9..0.
    pub true_peak_db: Option<f64>,
    /// Apply master loudness normalization. Default true. When false, the true-peak limiter still runs as a clip guard.
    pub normalize: Option<bool>,
    /// Export subtitles: true (= vtt + srt), false, or a list of formats from vtt, srt, json (captions + word
    /// timestamps) and transcript (HyperFrames-compatible word array).
    pub export_subtitles: Option<SubtitleExport>,
    /// Export one 32-bit float WAV stem per track to `<output stem>.stems/`. Default false.
    pub export_stems: Option<bool>,
    /// Write the render report to `<output stem>.manifest.json`. Default false.
    pub export_manifest: Option<bool>,
    /// Time after the last finite item (speech, non-looping clips) before the end. Default: the longest
    /// `fade_out_ms` among looping/open-ended beds, else 0.
    pub tail_ms: Option<Millis>,
    /// Force the total duration. Content beyond it is cut.
    pub duration_ms: Option<Millis>,
    /// TPDF dither when writing 16-bit PCM. Default true.
    pub dither: Option<bool>,
}

/// Caption layout settings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct SubtitleSpec {
    /// Maximum characters per caption line. Default 42.
    pub max_chars_per_line: Option<u32>,
    /// Maximum lines per caption. Default 2.
    pub max_lines: Option<u32>,
    /// Maximum on-screen duration of one caption. Default 6000 ms.
    pub max_duration_ms: Option<Millis>,
    /// Minimum on-screen duration of one caption (extended when possible). Default 800 ms.
    pub min_duration_ms: Option<Millis>,
    /// Prefix captions with speaker labels. Default: on when there are 2+ distinct speakers.
    pub speaker_labels: Option<bool>,
}

/// Timing and overlap policy.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct TimingSpec {
    /// What to do when speech cues overlap: push (default; moves implicitly-placed cues later), warn, error, allow.
    #[schemars(with = "Option<OverlapPolicyName>")]
    pub speech_overlap: Option<String>,
    /// Gap inserted when `push` moves a cue that has no explicit preceding pause. Default 250 ms.
    pub min_gap_ms: Option<Millis>,
    /// Overlaps shorter than this are ignored. Default 20 ms.
    pub overlap_tolerance_ms: Option<Millis>,
}

/// Mix-wide settings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct MixSpec {
    /// Source-level normalization target in LUFS for tracks with `normalize` enabled. Default -20.
    pub reference_lufs: Option<f64>,
}

/// Voice settings. Used for `defaults`, entries of `speakers`, and inline on speech tracks and cues.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct VoiceSpec {
    /// Provider instance name (a key of `providers`, or a built-in provider name).
    pub provider: Option<String>,
    /// Provider-specific voice identifier (e.g. an ElevenLabs voice id, an OpenAI voice name, a Kokoro voice like am_adam).
    pub voice_id: Option<String>,
    /// Provider model id (e.g. eleven_multilingual_v2, gpt-4o-mini-tts, sonic-2).
    pub model: Option<String>,
    /// Language code passed to providers that support it (e.g. "en").
    pub language: Option<String>,
    /// Provider-specific voice settings (e.g. ElevenLabs stability/similarity_boost, OpenAI instructions, speed).
    pub settings: Option<Map<String, Value>>,
    /// Extra gain in dB for this speaker's cues.
    pub gain_db: Option<f64>,
    /// Display name used in subtitles. Default: the speaker key.
    pub label: Option<String>,
}

/// A named TTS provider instance.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct ProviderSpec {
    /// Built-in provider type: elevenlabs, openai, cartesia, kokoro, piper, macos_say, command or mock.
    /// Defaults to the instance name when that is a built-in name or alias.
    #[serde(rename = "type")]
    #[schemars(with = "Option<ProviderTypeName>")]
    pub kind: Option<String>,
    /// Name of the environment variable that holds the API key (never put keys in the timeline).
    pub api_key_env: Option<String>,
    /// API base URL override.
    pub base_url: Option<String>,
    /// Default model for this instance.
    pub model: Option<String>,
    /// Provider-native audio format override (e.g. ElevenLabs "mp3_44100_128").
    pub output_format: Option<String>,
    /// Maximum parallel requests to this instance.
    pub max_concurrency: Option<u32>,
    /// Per-request timeout in milliseconds. Default 120000.
    pub timeout_ms: Option<u64>,
    /// Speaking rate (characters per second, spaces and punctuation included) assumed for dry-run duration
    /// estimates. Preferred over `words_per_minute`.
    pub chars_per_second: Option<f64>,
    /// Speaking rate (words per minute) assumed for dry-run estimates when `chars_per_second` is not set.
    pub words_per_minute: Option<f64>,
    /// Default voice settings for cues using this instance.
    pub settings: Option<Map<String, Value>>,
    /// argv template for `command`/`piper` providers. Placeholders: {text}, {text_file}, {output}, {voice_id}, {model}, {language}.
    pub command: Option<Vec<String>>,
    /// Template written to the child's stdin for `command`/`piper` providers, e.g. "{text}".
    pub stdin: Option<String>,
    /// Directory searched for `<voice_id>.onnx` (piper).
    pub voices_dir: Option<String>,
}

/// A track. `type` selects which fields apply: speech tracks use `sequence` and voice fields; music and sfx
/// tracks use either clip fields directly (single-clip shorthand) or a `clips` list.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct Track {
    /// Unique id (shared namespace with cue and clip ids).
    pub id: Option<String>,
    /// Track type: speech, music or sfx.
    #[serde(rename = "type")]
    #[schemars(with = "Option<TrackKindName>")]
    pub kind: Option<String>,
    /// Linear fader gain (1.0 = unity). Default 1.0.
    pub volume: Option<f64>,
    /// Extra fader gain in dB. Default 0.
    pub gain_db: Option<f64>,
    /// Stereo position from -1 (left) to 1 (right). Default 0.
    pub pan: Option<f64>,
    /// Exclude the track from the mix (it is still laid out). Default false.
    pub mute: Option<bool>,
    /// Source-level loudness normalization: true/false or {target_lufs}. Default: speech and music true, sfx false.
    pub normalize: Option<NormalizeInput>,
    /// Absolute start of the track's first item (when that item has no explicit start).
    pub start_at_ms: Option<Millis>,
    /// Relative start of the track's first item.
    pub start_relative: Option<Anchor>,
    /// Duck this track while other tracks are active.
    pub ducking: Option<DuckingSpec>,

    // ---- speech tracks -------------------------------------------------------------------------------
    /// (speech) Ordered cues and pauses.
    pub sequence: Option<Vec<SequenceItem>>,
    /// (speech) Speaker name for all cues of this track.
    pub speaker: Option<String>,
    /// (speech) Provider instance for all cues of this track.
    pub provider: Option<String>,
    /// (speech) Voice id for all cues of this track.
    pub voice_id: Option<String>,
    /// (speech) Model for all cues of this track.
    pub model: Option<String>,
    /// (speech) Language for all cues of this track.
    pub language: Option<String>,
    /// (speech) Provider-specific voice settings for all cues of this track.
    pub settings: Option<Map<String, Value>>,
    /// (speech) Trim leading/trailing silence from synthesized cues: true/false or {threshold_db, padding_ms}. Default true.
    pub trim_silence: Option<TrimSilenceInput>,

    // ---- music / sfx tracks: single-clip shorthand ---------------------------------------------------
    /// (music/sfx) Audio file path, relative to the timeline file.
    pub source: Option<String>,
    /// (music/sfx) Loop the source to fill the clip length (open-ended loops run until the timeline end).
    #[serde(rename = "loop")]
    pub looped: Option<bool>,
    /// (music/sfx) Equal-power crossfade at loop seams. Default 0.
    pub loop_crossfade_ms: Option<Millis>,
    /// (music/sfx) Fade-in length. Default 0.
    pub fade_in_ms: Option<Millis>,
    /// (music/sfx) Fade-out length. Default 0.
    pub fade_out_ms: Option<Millis>,
    /// (music/sfx) Fade shape: equal_power (default), linear, exponential.
    #[schemars(with = "Option<FadeCurveName>")]
    pub fade_curve: Option<String>,
    /// (music/sfx) Offset into the source where playback begins. Default 0.
    pub trim_start_ms: Option<Millis>,
    /// (music/sfx) Play length.
    pub duration_ms: Option<Millis>,
    /// (music/sfx) Absolute end time.
    pub end_at_ms: Option<Millis>,
    /// (music/sfx) Relative end time.
    pub end_relative: Option<Anchor>,
    /// (music/sfx) Multiple clips placed on this track (alternative to the single-clip shorthand).
    pub clips: Option<Vec<Clip>>,
}

/// An item of a speech track's `sequence`: either a pause (`pause_duration` or `pause`) or a cue (`text` + optional fields).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct SequenceItem {
    // ---- pause -----------------------------------------------------------------------------------------
    /// Pause length (pause items only), e.g. 350 or "350ms".
    pub pause_duration: Option<Millis>,
    /// Alias of `pause_duration`.
    pub pause: Option<Millis>,

    // ---- cue -------------------------------------------------------------------------------------------
    /// Cue id. Default `<track_id>.cue<n>`.
    pub id: Option<String>,
    /// Text to speak. Required for cues.
    pub text: Option<String>,
    /// Subtitle text override (defaults to `text`).
    pub caption: Option<String>,
    /// Include this cue in subtitle files. Default true.
    pub subtitles: Option<bool>,
    /// Speaker name (see `speakers`).
    pub speaker: Option<String>,
    /// Provider instance override.
    pub provider: Option<String>,
    /// Voice id override.
    pub voice_id: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Language override.
    pub language: Option<String>,
    /// Provider-specific voice settings (merged over speaker/track/defaults settings).
    pub settings: Option<Map<String, Value>>,
    /// Absolute start time. Without a start the cue follows the previous item of its track.
    pub start_at_ms: Option<Millis>,
    /// Start relative to another cue, clip or track.
    pub start_relative: Option<Anchor>,
    /// Per-cue linear gain. Default 1.0.
    pub volume: Option<f64>,
    /// Per-cue gain in dB. Default 0.
    pub gain_db: Option<f64>,
    /// Allow this cue to overlap other speech (intentional crosstalk). Default false.
    pub allow_overlap: Option<bool>,
}

/// A clip on a music or sfx track.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct Clip {
    /// Clip id. Default `<track_id>.clip<n>`.
    pub id: Option<String>,
    /// Audio file path, relative to the timeline file. Required.
    pub source: Option<String>,
    /// Absolute start time. Without a start the clip follows the previous clip (minus `crossfade_ms`).
    pub start_at_ms: Option<Millis>,
    /// Start relative to a cue, clip or track.
    pub start_relative: Option<Anchor>,
    /// Offset into the source where playback begins. Default 0.
    pub trim_start_ms: Option<Millis>,
    /// Play length.
    pub duration_ms: Option<Millis>,
    /// Absolute end time.
    pub end_at_ms: Option<Millis>,
    /// Relative end time.
    pub end_relative: Option<Anchor>,
    /// Loop the source to fill the clip length.
    #[serde(rename = "loop")]
    pub looped: Option<bool>,
    /// Equal-power crossfade at loop seams. Default 0.
    pub loop_crossfade_ms: Option<Millis>,
    /// Fade-in length. Default 0.
    pub fade_in_ms: Option<Millis>,
    /// Fade-out length. Default 0.
    pub fade_out_ms: Option<Millis>,
    /// Fade shape: equal_power (default), linear, exponential.
    #[schemars(with = "Option<FadeCurveName>")]
    pub fade_curve: Option<String>,
    /// Overlap with the previous clip of this track, with equal-power fades on both sides. Default 0. Only a
    /// clip without its own start (no `start_at_ms`/`start_relative`) crossfades; elsewhere it is ignored.
    pub crossfade_ms: Option<Millis>,
    /// Per-clip linear gain. Default 1.0.
    pub volume: Option<f64>,
    /// Per-clip gain in dB. Default 0.
    pub gain_db: Option<f64>,
}

/// A relative time anchor. Exactly one of `after_cue`, `with_cue`, `before_cue`.
/// The referenced id may be a cue, a clip or a track.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct Anchor {
    /// Anchor at the END of this item: start = end(ref) + offset.
    pub after_cue: Option<String>,
    /// Anchor at the START of this item: start = start(ref) + offset.
    pub with_cue: Option<String>,
    /// (start_relative only) This item ENDS at start(ref) + offset.
    pub before_cue: Option<String>,
    /// Signed offset added to the anchor time. Default 0.
    pub offset_ms: Option<Millis>,
}

/// Sidechain ducking of a track while trigger tracks are active.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct DuckingSpec {
    /// Track ids whose activity ducks this track. Required.
    pub trigger_tracks: Option<Vec<String>>,
    /// Attenuation depth in dB (sign ignored: -18 and 18 both attenuate by 18 dB). Default -15.
    pub reduction_db: Option<f64>,
    /// Ramp time from 0 dB to full depth. Default 80 ms.
    pub attack_ms: Option<Millis>,
    /// Ramp time from full depth back to 0 dB. Default 400 ms.
    pub release_ms: Option<Millis>,
    /// Gaps shorter than this keep the duck engaged; activity is held this long. Default 250 ms.
    pub hold_ms: Option<Millis>,
    /// Detect activity this much earlier so the bed is fully ducked when speech begins. Default = attack_ms.
    pub lookahead_ms: Option<Millis>,
    /// RMS threshold in dBFS for sidechain mode. Default -40.
    pub threshold_db: Option<f64>,
    /// sidechain (default, signal-driven) or cue (timeline-driven).
    #[schemars(with = "Option<DuckingModeName>")]
    pub mode: Option<String>,
}

// ---- enum vocabularies ------------------------------------------------------------------------------------
//
// Enum-valued fields are strings in the raw AST (see module docs). These enums give the JSON Schema its
// `enum` lists and give lowering a single place to parse names, aliases and did-you-mean candidates.
// Spellings are strict (DESIGN.md §2.1): only the listed names and aliases, exactly as written, so the
// schema and the compiler accept the same strings.

macro_rules! name_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $text:literal $(| $alias:literal)*),+ $(,)? }) => {
        $(#[$meta])*
        // Variants are documented by their serialized names (`NAMES`); doc comments on variants would change
        // the generated JSON Schema from a plain `enum` list to `oneOf` + `const`.
        #[allow(missing_docs)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, serde::Serialize, JsonSchema)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            /// Canonical names, in declaration order (used for did-you-mean hints and docs).
            pub const NAMES: &'static [&'static str] = &[$($text),+];

            /// Every accepted spelling: the canonical names (in declaration order) followed by all
            /// aliases. These are exactly the strings [`Self::parse`] accepts (and the `enum` of the JSON
            /// Schema).
            pub const SPELLINGS: &'static [&'static str] = &[$($text),+ $($(, $alias)*)+];

            /// Parse a canonical name or alias, spelled exactly as listed in [`Self::SPELLINGS`]
            /// (case-sensitive; no whitespace trimming). Case and separator variants such as `Speech`,
            /// `MP3` or `equal-power` are not accepted; [`Self::spelling_of`] finds the intended name for
            /// a did-you-mean hint.
            pub fn parse(s: &str) -> Option<Self> {
                $(
                    if s == $text $(|| s == $alias)* {
                        return Some(Self::$variant);
                    }
                )+
                None
            }

            /// The accepted spelling that `s` differs from only by case, separators (`-`, ` `, `_`) or
            /// surrounding whitespace: `"MP3"` → `mp3`, `"Equal-Power"` → `equal_power`, `"WebVTT"` →
            /// `webvtt`. None when `s` is already accepted or matches nothing.
            pub fn spelling_of(s: &str) -> Option<&'static str> {
                if Self::parse(s).is_some() {
                    return None;
                }
                let squash = |t: &str| -> String {
                    t.trim()
                        .chars()
                        .filter(|c| !matches!(c, '-' | '_' | ' '))
                        .flat_map(char::to_lowercase)
                        .collect()
                };
                let wanted = squash(s);
                Self::SPELLINGS.iter().copied().find(|spelling| squash(spelling) == wanted)
            }

            /// The canonical name.
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

name_enum!(
    /// Track type.
    TrackKindName { Speech = "speech", Music = "music", Sfx = "sfx" }
);

name_enum!(
    /// Output container/codec.
    OutputFormatName {
        Wav = "wav" | "wave",
        Mp3 = "mp3",
        Flac = "flac",
        Ogg = "ogg" | "vorbis",
        Opus = "opus",
        M4a = "m4a" | "aac" | "mp4",
    }
);

name_enum!(
    /// Master loudness preset.
    LoudnessPresetName {
        Podcast = "podcast",
        Streaming = "streaming",
        Broadcast = "broadcast" | "ebu_r128" | "ebu",
        Atsc = "atsc" | "atsc_a85" | "a85",
    }
);

name_enum!(
    /// Speech overlap policy.
    OverlapPolicyName { Push = "push", Warn = "warn", Error = "error", Allow = "allow" }
);

name_enum!(
    /// Fade curve shape.
    FadeCurveName {
        EqualPower = "equal_power" | "equalpower" | "sine" | "constant_power",
        Linear = "linear",
        Exponential = "exponential" | "exp" | "log" | "logarithmic",
    }
);

name_enum!(
    /// Ducking detector mode.
    DuckingModeName { Sidechain = "sidechain", Cue = "cue" | "timeline" }
);

name_enum!(
    /// Subtitle export format.
    SubtitleFormatName {
        Vtt = "vtt" | "webvtt",
        Srt = "srt",
        Json = "json" | "words",
        Transcript = "transcript" | "hyperframes",
    }
);

name_enum!(
    /// Built-in TTS provider type.
    ProviderTypeName {
        Elevenlabs = "elevenlabs" | "eleven_labs" | "11labs",
        Openai = "openai" | "open_ai",
        Cartesia = "cartesia",
        Kokoro = "kokoro" | "local_kokoro" | "kokoro_fastapi",
        Piper = "piper",
        MacosSay = "macos_say" | "say" | "system",
        Command = "command",
        Mock = "mock",
    }
);
