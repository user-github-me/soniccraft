//! Static knowledge about built-in TTS provider types: defaults, requirements and limits. Used by
//! lowering/validation (core), by the TTS crate and by the engine's environment checks.
//!
//! Values follow docs/DESIGN.md §2.7, §2.8, §5.2 and §5.3 and the provider documentation as of
//! 2026-10 (ElevenLabs per-model request limits, OpenAI's 4096-character `input` cap, Cartesia's
//! `sonic-3.6` default model, Kokoro-FastAPI's captioned endpoint for word timings).

use crate::ir::ProviderType;

/// Static metadata for a built-in provider type.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderMeta {
    /// The provider type this entry describes.
    pub provider_type: ProviderType,
    /// Human-readable name, e.g. "ElevenLabs".
    pub display_name: &'static str,
    /// Default env var holding the API key (None = no key needed).
    pub default_api_key_env: Option<&'static str>,
    /// Env var that overrides the default base URL, if any (e.g. KOKORO_BASE_URL).
    pub base_url_env: Option<&'static str>,
    /// API base URL used when neither the timeline nor [`ProviderMeta::base_url_env`] sets one
    /// (None for providers that run a local process).
    pub default_base_url: Option<&'static str>,
    /// Model used when no voice layer sets `model` (None = the provider has no model concept).
    pub default_model: Option<&'static str>,
    /// Voice used when none resolves (None = `voice_id` is required).
    pub default_voice: Option<&'static str>,
    /// Parallel requests per provider instance unless `max_concurrency` is set (DESIGN.md §2.8).
    pub default_max_concurrency: usize,
    /// Speaking-rate assumption for dry-run estimates, in characters per second (spaces and punctuation
    /// included). See DESIGN.md §5.3 and [`default_chars_per_second`] for model-specific rates.
    pub default_chars_per_second: f64,
    /// Whether the provider returns word timings.
    pub word_timestamps: bool,
    /// Runs locally (no network/API cost).
    pub local: bool,
    /// Short note for `soniccraft providers` / MCP list_providers.
    pub notes: &'static str,
}

/// Default per-request timeout for every provider instance (`timeout_ms`), in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Characters-per-second assumption for ElevenLabs' expressive `eleven_v3` / `eleven_v4*` models.
const ELEVENLABS_EXPRESSIVE_CPS: f64 = 14.0;

/// Conservative request limit for ElevenLabs models this table does not know.
const ELEVENLABS_UNKNOWN_MODEL_LIMIT: usize = 5_000;

static METAS: [ProviderMeta; 8] = [
    ProviderMeta {
        provider_type: ProviderType::Elevenlabs,
        display_name: "ElevenLabs",
        default_api_key_env: Some("ELEVENLABS_API_KEY"),
        base_url_env: None,
        default_base_url: Some("https://api.elevenlabs.io"),
        default_model: Some("eleven_multilingual_v2"),
        default_voice: None,
        default_max_concurrency: 2,
        default_chars_per_second: 15.5,
        word_timestamps: true,
        local: false,
        notes: "Cloud TTS with character-level timestamps. `voice_id` is required; eleven_v4 models accept only \
                stability and similarity_boost settings.",
    },
    ProviderMeta {
        provider_type: ProviderType::Openai,
        display_name: "OpenAI",
        default_api_key_env: Some("OPENAI_API_KEY"),
        base_url_env: None,
        default_base_url: Some("https://api.openai.com/v1"),
        default_model: Some("gpt-4o-mini-tts"),
        default_voice: Some("alloy"),
        default_max_concurrency: 4,
        default_chars_per_second: 15.0,
        word_timestamps: false,
        local: false,
        notes: "OpenAI-compatible /audio/speech client; OpenAI's TTS models retire 2027-01-06 — point base_url at \
                any compatible server",
    },
    ProviderMeta {
        provider_type: ProviderType::Cartesia,
        display_name: "Cartesia",
        default_api_key_env: Some("CARTESIA_API_KEY"),
        base_url_env: None,
        default_base_url: Some("https://api.cartesia.ai"),
        default_model: Some("sonic-3.6"),
        default_voice: None,
        default_max_concurrency: 2,
        default_chars_per_second: 15.0,
        word_timestamps: true,
        local: false,
        notes: "Cloud TTS (Sonic) with word timestamps over SSE. `voice_id` is required.",
    },
    ProviderMeta {
        provider_type: ProviderType::Kokoro,
        display_name: "Kokoro",
        default_api_key_env: None,
        base_url_env: Some("KOKORO_BASE_URL"),
        default_base_url: Some("http://localhost:8880"),
        default_model: Some("kokoro"),
        default_voice: Some("af_heart"),
        default_max_concurrency: 1,
        default_chars_per_second: 16.0,
        word_timestamps: true,
        local: true,
        notes: "Local Kokoro-FastAPI server (word timestamps via its captioned endpoint); base URL from \
                KOKORO_BASE_URL, optional KOKORO_API_KEY.",
    },
    ProviderMeta {
        provider_type: ProviderType::Piper,
        display_name: "Piper",
        default_api_key_env: None,
        base_url_env: None,
        default_base_url: None,
        default_model: None,
        default_voice: None,
        default_max_concurrency: 2,
        default_chars_per_second: 17.0,
        word_timestamps: false,
        local: true,
        notes: "Local `piper` command; `voice_id` is a .onnx model path or a voice name looked up in voices_dir \
                (env PIPER_VOICES_DIR).",
    },
    ProviderMeta {
        provider_type: ProviderType::MacosSay,
        display_name: "macOS say",
        default_api_key_env: None,
        base_url_env: None,
        default_base_url: None,
        default_model: None,
        default_voice: Some("Samantha"),
        default_max_concurrency: 4,
        default_chars_per_second: 17.0,
        word_timestamps: false,
        local: true,
        notes: "macOS only",
    },
    ProviderMeta {
        provider_type: ProviderType::Command,
        display_name: "Custom command",
        default_api_key_env: None,
        base_url_env: None,
        default_base_url: None,
        default_model: None,
        default_voice: None,
        default_max_concurrency: 2,
        default_chars_per_second: 15.0,
        word_timestamps: false,
        local: true,
        notes: "Runs the `command` argv template of the provider instance; placeholders {text}, {text_file}, \
                {output}, {voice_id}, {model}, {language}.",
    },
    ProviderMeta {
        provider_type: ProviderType::Mock,
        display_name: "Mock",
        default_api_key_env: None,
        base_url_env: None,
        default_base_url: None,
        default_model: None,
        default_voice: Some("mock"),
        default_max_concurrency: 8,
        default_chars_per_second: 15.0,
        word_timestamps: true,
        local: true,
        notes: "Deterministic offline test voice (synthetic tone bursts with exact word timings); no setup needed.",
    },
];

static ALL: [ProviderType; 8] = [
    ProviderType::Elevenlabs,
    ProviderType::Openai,
    ProviderType::Cartesia,
    ProviderType::Kokoro,
    ProviderType::Piper,
    ProviderType::MacosSay,
    ProviderType::Command,
    ProviderType::Mock,
];

/// Metadata for a provider type.
pub fn meta(provider_type: ProviderType) -> &'static ProviderMeta {
    let index = match provider_type {
        ProviderType::Elevenlabs => 0,
        ProviderType::Openai => 1,
        ProviderType::Cartesia => 2,
        ProviderType::Kokoro => 3,
        ProviderType::Piper => 4,
        ProviderType::MacosSay => 5,
        ProviderType::Command => 6,
        ProviderType::Mock => 7,
    };
    &METAS[index]
}

/// All built-in provider types, in display order.
pub fn all() -> &'static [ProviderType] {
    &ALL
}

/// Maximum characters per request for `provider_type` with `model` (None = no known limit).
///
/// `model: None` means the provider's default model. ElevenLabs limits are per model (2026-10:
/// `eleven_v4` 10 000, `eleven_v3` 5 000, `eleven_multilingual_v2` 10 000, `eleven_flash_v2_5` 40 000,
/// `eleven_flash_v2` 30 000; the deprecated turbo models match their flash equivalents); unknown
/// ElevenLabs models get a conservative 5 000. OpenAI's `/audio/speech` caps `input` at 4096 characters.
/// Cartesia documents no limit; Kokoro's limit is a server setting; local engines have none.
pub fn max_text_chars(provider_type: ProviderType, model: Option<&str>) -> Option<usize> {
    match provider_type {
        ProviderType::Elevenlabs => {
            let model = model
                .or(meta(provider_type).default_model)
                .unwrap_or_default();
            Some(match model.trim() {
                "eleven_v4" => 10_000,
                "eleven_v3" => 5_000,
                "eleven_multilingual_v2" | "eleven_multilingual_v1" => 10_000,
                "eleven_flash_v2_5" | "eleven_turbo_v2_5" => 40_000,
                "eleven_flash_v2" | "eleven_turbo_v2" => 30_000,
                _ => ELEVENLABS_UNKNOWN_MODEL_LIMIT,
            })
        }
        ProviderType::Openai => Some(4_096),
        ProviderType::Cartesia
        | ProviderType::Kokoro
        | ProviderType::Piper
        | ProviderType::MacosSay
        | ProviderType::Command
        | ProviderType::Mock => None,
    }
}

/// Default speaking rate in characters per second for `provider_type` with `model` (None = the
/// provider's default model), used by dry-run duration estimates (DESIGN.md §5.3).
///
/// ElevenLabs' expressive `eleven_v3*` / `eleven_v4*` models speak slower (14 cps) than the v2/flash
/// family (15.5 cps); every other provider uses [`ProviderMeta::default_chars_per_second`].
pub fn default_chars_per_second(provider_type: ProviderType, model: Option<&str>) -> f64 {
    let meta = meta(provider_type);
    if provider_type == ProviderType::Elevenlabs {
        let model = model.or(meta.default_model).unwrap_or_default().trim();
        if model.starts_with("eleven_v3") || model.starts_with("eleven_v4") {
            return ELEVENLABS_EXPRESSIVE_CPS;
        }
    }
    meta.default_chars_per_second
}

/// Resolve a provider name written in a timeline (`provider: local_kokoro`) to a built-in type, using
/// the canonical names and aliases of DESIGN.md §2.8, spelled exactly (case-sensitive, `-` is not `_`;
/// [`ProviderType::spelling_of`] finds the intended name of a case or separator variant).
pub fn builtin_type(name: &str) -> Option<ProviderType> {
    ProviderType::parse(name)
}

/// Every spelling accepted for a built-in provider name (canonical names first, then aliases), for
/// did-you-mean hints.
pub fn builtin_names() -> &'static [&'static str] {
    ProviderType::SPELLINGS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_has_matching_meta() {
        assert_eq!(all().len(), 8);
        for &t in all() {
            assert_eq!(meta(t).provider_type, t, "{t}");
            assert!(meta(t).default_max_concurrency >= 1);
            assert!(meta(t).default_chars_per_second > 0.0);
            assert!(!meta(t).display_name.is_empty());
            assert!(!meta(t).notes.is_empty());
        }
    }

    #[test]
    fn display_order_matches_design() {
        let names: Vec<&str> = all().iter().map(|t| t.as_str()).collect();
        assert_eq!(
            names,
            [
                "elevenlabs",
                "openai",
                "cartesia",
                "kokoro",
                "piper",
                "macos_say",
                "command",
                "mock"
            ]
        );
    }

    #[test]
    fn defaults_match_design() {
        let el = meta(ProviderType::Elevenlabs);
        assert_eq!(el.default_api_key_env, Some("ELEVENLABS_API_KEY"));
        assert_eq!(el.default_base_url, Some("https://api.elevenlabs.io"));
        assert_eq!(el.default_model, Some("eleven_multilingual_v2"));
        assert_eq!(el.default_voice, None);
        assert_eq!(el.default_max_concurrency, 2);
        assert!(el.word_timestamps && !el.local);

        let oa = meta(ProviderType::Openai);
        assert_eq!(oa.default_api_key_env, Some("OPENAI_API_KEY"));
        assert_eq!(oa.default_base_url, Some("https://api.openai.com/v1"));
        assert_eq!(oa.default_model, Some("gpt-4o-mini-tts"));
        assert_eq!(oa.default_voice, Some("alloy"));
        assert_eq!(oa.default_max_concurrency, 4);
        assert!(!oa.word_timestamps);
        assert!(oa.notes.contains("2027-01-06"));

        let ca = meta(ProviderType::Cartesia);
        assert_eq!(ca.default_api_key_env, Some("CARTESIA_API_KEY"));
        assert_eq!(ca.default_base_url, Some("https://api.cartesia.ai"));
        assert_eq!(ca.default_model, Some("sonic-3.6"));
        assert_eq!(ca.default_max_concurrency, 2);

        let ko = meta(ProviderType::Kokoro);
        assert_eq!(ko.default_api_key_env, None);
        assert_eq!(ko.base_url_env, Some("KOKORO_BASE_URL"));
        assert_eq!(ko.default_base_url, Some("http://localhost:8880"));
        assert_eq!(ko.default_voice, Some("af_heart"));
        assert_eq!(ko.default_max_concurrency, 1);
        assert!(ko.local && ko.word_timestamps);

        assert_eq!(meta(ProviderType::Piper).default_voice, None);
        assert_eq!(meta(ProviderType::MacosSay).default_voice, Some("Samantha"));
        assert_eq!(meta(ProviderType::MacosSay).notes, "macOS only");
        assert_eq!(meta(ProviderType::Command).default_voice, None);
        assert_eq!(meta(ProviderType::Mock).default_voice, Some("mock"));
        assert_eq!(meta(ProviderType::Mock).default_max_concurrency, 8);
        assert_eq!(meta(ProviderType::MacosSay).default_max_concurrency, 4);
        assert_eq!(meta(ProviderType::Piper).default_max_concurrency, 2);
        assert_eq!(meta(ProviderType::Command).default_max_concurrency, 2);
    }

    #[test]
    fn chars_per_second() {
        use ProviderType::*;
        assert_eq!(default_chars_per_second(Elevenlabs, None), 15.5);
        assert_eq!(
            default_chars_per_second(Elevenlabs, Some("eleven_flash_v2_5")),
            15.5
        );
        assert_eq!(
            default_chars_per_second(Elevenlabs, Some("eleven_v3")),
            14.0
        );
        assert_eq!(
            default_chars_per_second(Elevenlabs, Some("eleven_v4")),
            14.0
        );
        assert_eq!(
            default_chars_per_second(Elevenlabs, Some("eleven_v4_turbo")),
            14.0
        );
        assert_eq!(default_chars_per_second(Openai, None), 15.0);
        assert_eq!(default_chars_per_second(Cartesia, Some("sonic-3.6")), 15.0);
        assert_eq!(default_chars_per_second(Kokoro, None), 16.0);
        assert_eq!(default_chars_per_second(Piper, None), 17.0);
        assert_eq!(default_chars_per_second(MacosSay, None), 17.0);
        assert_eq!(default_chars_per_second(Command, None), 15.0);
    }

    #[test]
    fn text_limits() {
        use ProviderType::*;
        assert_eq!(max_text_chars(Elevenlabs, None), Some(10_000));
        assert_eq!(max_text_chars(Elevenlabs, Some("eleven_v4")), Some(10_000));
        assert_eq!(max_text_chars(Elevenlabs, Some("eleven_v3")), Some(5_000));
        assert_eq!(
            max_text_chars(Elevenlabs, Some("eleven_multilingual_v2")),
            Some(10_000)
        );
        assert_eq!(
            max_text_chars(Elevenlabs, Some("eleven_flash_v2_5")),
            Some(40_000)
        );
        assert_eq!(
            max_text_chars(Elevenlabs, Some("eleven_flash_v2")),
            Some(30_000)
        );
        assert_eq!(
            max_text_chars(Elevenlabs, Some("eleven_turbo_v2_5")),
            Some(40_000)
        );
        assert_eq!(
            max_text_chars(Elevenlabs, Some("eleven_turbo_v2")),
            Some(30_000)
        );
        assert_eq!(
            max_text_chars(Elevenlabs, Some("eleven_future_v9")),
            Some(5_000)
        );
        assert_eq!(max_text_chars(Openai, Some("tts-1")), Some(4_096));
        for t in [Cartesia, Kokoro, Piper, MacosSay, Command, Mock] {
            assert_eq!(max_text_chars(t, None), None, "{t}");
        }
    }

    #[test]
    fn builtin_name_resolution() {
        assert_eq!(builtin_type("local_kokoro"), Some(ProviderType::Kokoro));
        assert_eq!(builtin_type("11labs"), Some(ProviderType::Elevenlabs));
        assert_eq!(builtin_type("eleven_labs"), Some(ProviderType::Elevenlabs));
        // Spellings are exact: case and `-` variants are left to the did-you-mean (`spelling_of`).
        assert_eq!(builtin_type("Eleven-Labs"), None);
        assert_eq!(builtin_type("say"), Some(ProviderType::MacosSay));
        assert_eq!(builtin_type("polly"), None);
        assert!(builtin_names().contains(&"kokoro_fastapi"));
        assert_eq!(&builtin_names()[..8], ProviderType::NAMES);
    }
}
