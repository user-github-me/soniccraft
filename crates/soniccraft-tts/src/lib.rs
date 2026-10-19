//! SonicCraft TTS: turns speech cues into audio (DESIGN.md §5).
//!
//! - **Providers**: ElevenLabs (`/with-timestamps`, character alignment → words), any OpenAI-compatible
//!   `/audio/speech` server, Cartesia (SSE with word timestamps), Kokoro-FastAPI (captioned speech with word
//!   timestamps, falling back to `/v1/audio/speech`), Piper and macOS `say` (child processes), an arbitrary
//!   `command`, and `mock`, a deterministic in-process synthetic voice with exact word timings.
//! - **Cache**: content-addressed by BLAKE3 over canonical JSON; `<key>.wav` (32-bit float, native rate,
//!   untrimmed) + `<key>.json` (metadata and word timings), written atomically through unpredictable,
//!   exclusively created temporary files. A failed write is reported in
//!   [`SynthesisResult::cache_write_error`], apart from provider notes.
//! - **Estimates**: [`estimate_duration_secs`] for dry runs (exact for the mock voice).
//! - **Robustness**: retries with exponential backoff and `Retry-After` (seconds or HTTP-date) for 408/429/5xx
//!   and network errors, all within the cue's `timeout_ms`; a refused local server is not retried and fails the
//!   instance's remaining cues at once ([`error_codes::NOT_ATTEMPTED`]); a global `jobs` limit plus
//!   per-instance `max_concurrency`; size-capped responses and implausibly long audio refused
//!   ([`error_codes::IMPLAUSIBLE_DURATION`]); errors with actionable hints.
//! - **Security**: API keys go only to the provider's own host, this machine (the provider's own key
//!   variable only), or hosts listed in `SONICCRAFT_ALLOWED_HOSTS` ([`key_host_allowed`]); unexpanded `${VAR}` placeholders count as
//!   missing keys; redirects are never followed; keys and URL credentials are scrubbed from every message;
//!   child processes get no provider keys and are killed with their process group; command arguments that
//!   would start with `-` are refused ([`command_argument_warnings`]).
//! - **Environment**: offline [`TtsClient::credential_status`] and [`TtsClient::credential_hint`], online
//!   [`TtsClient::check_credentials_online`], [`probe_reachable`] for local servers,
//!   [`TtsClient::list_voices_in`], [`resolve_piper_model`] and [`resolve_macos_voice`] for validation,
//!   [`settings_keys`] (unread settings become `setting_ignored` notes, see [`voice_notes`]), and
//!   [`render_mock`] for exact dry-run durations of the mock voice.
//!
//! Requests are built from the lowered IR ([`soniccraft_core::ir::ProviderInstance`] and
//! [`soniccraft_core::ir::VoiceIr`]).
//!
//! ```
//! use serde_json::Map;
//! use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};
//! use soniccraft_tts::{SynthesisRequest, TtsClient, TtsOptions, estimate_duration_secs};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), soniccraft_tts::TtsError> {
//! let provider = ProviderInstance {
//!     name: "mock".into(), provider_type: ProviderType::Mock, configured: false, api_key_env: None,
//!     base_url: None, model: None, output_format: None, max_concurrency: 8, timeout_ms: 120_000,
//!     chars_per_second: None, words_per_minute: None, settings: Map::new(), command: None, stdin: None,
//!     voices_dir: None,
//! };
//! let voice = VoiceIr {
//!     provider: "mock".into(), provider_type: ProviderType::Mock, voice_id: Some("narrator".into()),
//!     model: None, language: None, settings: Map::new(),
//! };
//! let text = "Hello from SonicCraft.";
//! let estimate = estimate_duration_secs(&provider, &voice, text);
//!
//! let client = TtsClient::new(TtsOptions::default()); // no cache directory: caching disabled
//! let request = SynthesisRequest {
//!     cue_id: "intro".into(), provider, voice, text: text.into(), base_dir: ".".into(),
//! };
//! let result = client.synthesize(&request).await?;
//! assert_eq!(result.audio.duration_secs(), estimate); // the mock voice is estimated exactly
//! assert_eq!(result.words.map(|w| w.len()), Some(3));
//! # Ok(())
//! # }
//! ```

// `TtsError` deliberately carries rich, public context for agents (provider, message, code, hint, status,
// retry data: 128 bytes). Errors happen at most once per cue, and most `Ok` payloads (audio buffers, results)
// are as large, so boxing would only make the public field-based API clumsier.
#![allow(clippy::result_large_err)]

mod cache;
pub mod canonical;
mod client;
mod defaults;
mod env;
mod error;
mod estimate;
mod hosts;
mod http;
mod keys;
mod process;
mod providers;
mod settings;
mod sse;
pub mod text;
mod types;

#[cfg(test)]
mod test_util;

pub use cache::{
    CACHE_VERSION, CacheStats, cache_key, cache_stats, clear_cache, resolve_cache_dir,
};
pub use client::{TtsClient, TtsOptions, render_mock};
pub use defaults::CARTESIA_API_VERSION;
pub use env::EnvSource;
pub use error::{TtsError, TtsErrorKind, error_codes};
pub use estimate::estimate_duration_secs;
pub use hosts::{is_local_host, is_local_url, probe_reachable, redact_url};
pub use keys::{ALLOWED_HOSTS_ENV, key_host_allowed};
pub use providers::command::{CommandArgumentWarning, command_argument_warnings};
pub use providers::macos_say::{macos_installed_voices, resolve_macos_voice};
pub use providers::piper::{PiperModelNotFound, resolve_piper_model};
pub use providers::{endpoint_url, settings_keys, voice_notes};
pub use types::{
    CachedMeta, CredentialStatus, SynthesisProgress, SynthesisRequest, SynthesisResult, Voice,
    WordTiming,
};
