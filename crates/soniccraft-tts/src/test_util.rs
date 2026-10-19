//! Builders shared by unit tests.

use serde_json::{Map, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};

use crate::settings::Resolved;

/// A built-in instance with lowering's defaults.
pub(crate) fn instance(t: ProviderType) -> ProviderInstance {
    ProviderInstance {
        name: t.as_str().to_owned(),
        provider_type: t,
        configured: false,
        api_key_env: crate::defaults::default_api_key_env(t).map(str::to_owned),
        base_url: None,
        model: None,
        output_format: None,
        max_concurrency: 2,
        timeout_ms: 120_000,
        chars_per_second: None,
        words_per_minute: None,
        settings: Map::new(),
        command: None,
        stdin: None,
        voices_dir: None,
    }
}

/// A voice for `t`.
pub(crate) fn voice(
    t: ProviderType,
    voice_id: Option<&str>,
    model: Option<&str>,
    language: Option<&str>,
    settings: Value,
) -> VoiceIr {
    VoiceIr {
        provider: t.as_str().to_owned(),
        provider_type: t,
        voice_id: voice_id.map(str::to_owned),
        model: model.map(str::to_owned),
        language: language.map(str::to_owned),
        settings: settings.as_object().cloned().unwrap_or_default(),
    }
}

/// Resolved parameters for a default instance of `t` and the given voice.
pub(crate) fn resolved(
    t: ProviderType,
    voice_id: Option<&str>,
    model: Option<&str>,
    language: Option<&str>,
    settings: Value,
) -> Resolved {
    Resolved::new(&instance(t), &voice(t, voice_id, model, language, settings))
}
