//! Built-in provider defaults, used whenever a [`ProviderInstance`](soniccraft_core::ir::ProviderInstance) or
//! [`VoiceIr`](soniccraft_core::ir::VoiceIr) field is unset. The static values come from
//! [`soniccraft_core::providers`] (DESIGN.md §2.7, §2.8, §5.3); protocol constants live here.

use soniccraft_core::ir::ProviderType;
use soniccraft_core::providers;

/// The `Cartesia-Version` header sent with every Cartesia request (and part of Cartesia cache keys).
pub const CARTESIA_API_VERSION: &str = "2026-08-14";

/// Version of the mock voice algorithm, part of mock cache keys so algorithm changes never serve stale audio.
pub(crate) const MOCK_ALGORITHM_VERSION: &str = "mock-1";

/// Per-request timeout when the instance does not set one.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = providers::DEFAULT_TIMEOUT_MS;

/// ElevenLabs output format used by default and as the fallback for formats the plan does not allow.
pub(crate) const ELEVENLABS_DEFAULT_FORMAT: &str = "pcm_24000";

/// Kokoro-FastAPI root when neither the instance nor `KOKORO_BASE_URL` sets one.
pub(crate) const KOKORO_DEFAULT_ROOT: &str = "http://localhost:8880";

/// Environment variable overriding the Kokoro base URL.
pub(crate) const KOKORO_BASE_URL_ENV: &str = "KOKORO_BASE_URL";

/// Optional Kokoro key variable (DESIGN.md §2.8: "kokoro: none (optional KOKORO_API_KEY)").
const KOKORO_API_KEY_ENV: &str = "KOKORO_API_KEY";

/// Environment variable naming the default piper voices directory.
pub(crate) const PIPER_VOICES_DIR_ENV: &str = "PIPER_VOICES_DIR";

/// Human-readable provider name.
pub(crate) fn display_name(t: ProviderType) -> &'static str {
    providers::meta(t).display_name
}

/// Default API base URL (kokoro's also honours `KOKORO_BASE_URL`, see [`KOKORO_BASE_URL_ENV`]).
pub(crate) fn default_base_url(t: ProviderType) -> Option<&'static str> {
    providers::meta(t).default_base_url
}

/// Default environment variable holding the API key. For kokoro the key is optional.
pub(crate) fn default_api_key_env(t: ProviderType) -> Option<&'static str> {
    providers::meta(t).default_api_key_env.or(match t {
        ProviderType::Kokoro => Some(KOKORO_API_KEY_ENV),
        _ => None,
    })
}

/// Default model id.
pub(crate) fn default_model(t: ProviderType) -> Option<&'static str> {
    providers::meta(t).default_model
}

/// Voice used when none resolves (DESIGN.md §2.7). `None` = `voice_id` is required.
pub(crate) fn default_voice(t: ProviderType) -> Option<&'static str> {
    providers::meta(t).default_voice
}

/// Speaking rate assumed by dry-run estimates, in characters per second (DESIGN.md §5.3).
pub(crate) fn default_chars_per_second(t: ProviderType, model: Option<&str>) -> f64 {
    providers::default_chars_per_second(t, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cps_table_matches_design() {
        assert_eq!(
            default_chars_per_second(ProviderType::Elevenlabs, None),
            15.5
        );
        assert_eq!(
            default_chars_per_second(ProviderType::Elevenlabs, Some("eleven_v3")),
            14.0
        );
        assert_eq!(
            default_chars_per_second(ProviderType::Elevenlabs, Some("eleven_v4_turbo")),
            14.0
        );
        assert_eq!(default_chars_per_second(ProviderType::Openai, None), 15.0);
        assert_eq!(default_chars_per_second(ProviderType::Cartesia, None), 15.0);
        assert_eq!(default_chars_per_second(ProviderType::Kokoro, None), 16.0);
        assert_eq!(default_chars_per_second(ProviderType::Piper, None), 17.0);
        assert_eq!(default_chars_per_second(ProviderType::MacosSay, None), 17.0);
        assert_eq!(default_chars_per_second(ProviderType::Command, None), 15.0);
    }

    #[test]
    fn defaults_match_design_and_protocol_constants() {
        assert_eq!(default_voice(ProviderType::Openai), Some("alloy"));
        assert_eq!(default_voice(ProviderType::Kokoro), Some("af_heart"));
        assert_eq!(default_voice(ProviderType::MacosSay), Some("Samantha"));
        assert_eq!(default_voice(ProviderType::Mock), Some("mock"));
        assert_eq!(default_voice(ProviderType::Elevenlabs), None);
        assert_eq!(default_voice(ProviderType::Piper), None);
        assert_eq!(default_model(ProviderType::Cartesia), Some("sonic-3.6"));
        assert_eq!(
            default_api_key_env(ProviderType::Kokoro),
            Some("KOKORO_API_KEY")
        );
        assert_eq!(
            default_api_key_env(ProviderType::Elevenlabs),
            Some("ELEVENLABS_API_KEY")
        );
        assert_eq!(
            default_base_url(ProviderType::Kokoro),
            Some(KOKORO_DEFAULT_ROOT)
        );
        assert_eq!(
            providers::meta(ProviderType::Kokoro).base_url_env,
            Some(KOKORO_BASE_URL_ENV)
        );
        assert_eq!(DEFAULT_TIMEOUT_MS, 120_000);
    }
}
