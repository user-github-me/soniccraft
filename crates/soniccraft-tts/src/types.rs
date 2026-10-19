//! Request, result and status types of the TTS API.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use soniccraft_core::ir::{ProviderInstance, VoiceIr};
use soniccraft_dsp::AudioBuffer;

/// One speech cue to synthesize.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesisRequest {
    /// Cue id, used in logs, progress events and error messages.
    pub cue_id: String,
    /// The provider instance the cue's voice refers to (`Program::providers[voice.provider]`).
    pub provider: ProviderInstance,
    /// The cue's fully merged voice.
    pub voice: VoiceIr,
    /// Text sent to the provider (`CueIr::text`).
    pub text: String,
    /// Directory that relative paths (piper model files, `command` programs) resolve against, normally the
    /// timeline's directory. Child processes also run with this working directory.
    pub base_dir: PathBuf,
}

/// A word and when it is spoken, in seconds from the start of the provider audio (before any trimming).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordTiming {
    /// The word as the provider aligned it, with attached punctuation (e.g. `world.`).
    pub text: String,
    /// Start, in seconds.
    pub start_secs: f64,
    /// End, in seconds (≥ `start_secs`).
    pub end_secs: f64,
}

/// A synthesized (or cached) cue.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesisResult {
    /// Decoded provider audio at the provider's native sample rate, untrimmed.
    pub audio: AudioBuffer,
    /// Word timings from the provider (elevenlabs, cartesia, kokoro captioned endpoint, mock), clamped to the
    /// audio length. `None` when the provider returns none: callers estimate them (DESIGN.md §5.4).
    pub words: Option<Vec<WordTiming>>,
    /// Served from the on-disk cache without calling the provider.
    pub cache_hit: bool,
    /// The content hash identifying this request (DESIGN.md §5.1).
    pub cache_key: String,
    /// In [`crate::TtsClient::synthesize_all`]: index of the earlier request with the same cache key whose
    /// result this is a copy of (the provider was called, or the cache read, once for all of them).
    pub duplicate_of: Option<usize>,
    /// Informational notes for `setting_ignored` diagnostics, e.g. settings a model ignores, settings keys
    /// the provider does not read, or an output-format fallback.
    pub notes: Vec<String>,
    /// Why the result could not be stored in the cache (directory not writable, disk full, …), so later
    /// renders will synthesize it again; `None` when it was stored, or when caching is off or the result
    /// came from the cache. The engine reports it once per render as a `cache_write_failed` warning.
    pub cache_write_error: Option<String>,
}

impl SynthesisResult {
    /// Length of the audio in seconds.
    pub fn duration_secs(&self) -> f64 {
        self.audio.duration_secs()
    }
}

/// What the cache knows about a cue, read without decoding the audio.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CachedMeta {
    /// Number of frames of the cached audio.
    pub frames: u64,
    /// Native sample rate of the cached audio.
    pub sample_rate: u32,
    /// Channel count of the cached audio.
    pub channels: u16,
    /// `frames / sample_rate`.
    pub duration_secs: f64,
    /// Word timings stored with the audio, if the provider returned any.
    pub words: Option<Vec<WordTiming>>,
}

/// A voice offered by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Voice {
    /// The value to put in `voice_id`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Language or locale, when known (e.g. `en`, `en-GB`, `en_US`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Gender label, when the provider publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    /// Free-form description (accent, style, sample sentence, model file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Whether a provider instance can be used in this environment, checked offline (no network).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CredentialStatus {
    /// The API key environment variable is set.
    Ready,
    /// The API key environment variable is not set (or empty).
    Missing {
        /// The variable to set.
        env_var: String,
    },
    /// No credentials are needed and the provider's prerequisites are present.
    NotRequired,
    /// The provider cannot run here (wrong OS, binary not found, incomplete configuration).
    Unavailable {
        /// Why, with a fix where possible.
        reason: String,
    },
}

impl CredentialStatus {
    /// True for [`CredentialStatus::Ready`] and [`CredentialStatus::NotRequired`].
    pub fn is_usable(&self) -> bool {
        matches!(
            self,
            CredentialStatus::Ready | CredentialStatus::NotRequired
        )
    }
}

/// Progress of [`crate::TtsClient::synthesize_all`], reported once per request as it completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SynthesisProgress {
    /// Index of the request in the input.
    pub index: usize,
    /// The request's cue id.
    pub cue_id: String,
    /// Served from the cache.
    pub cache_hit: bool,
    /// False when the request failed.
    pub success: bool,
    /// Requests completed so far, including this one.
    pub done: usize,
    /// Total number of requests.
    pub total: usize,
}
