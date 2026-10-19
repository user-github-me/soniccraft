//! Built-in providers and the dispatch between them.

pub(crate) mod cartesia;
pub(crate) mod command;
pub(crate) mod elevenlabs;
pub(crate) mod kokoro;
pub(crate) mod macos_say;
pub(crate) mod mock;
pub(crate) mod openai;
pub(crate) mod piper;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;
use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};
use soniccraft_dsp::AudioBuffer;

use crate::env::EnvSource;
use crate::error::{TtsError, TtsErrorKind};
use crate::http::{HttpClients, RetryPolicy};
use crate::keys;
use crate::settings::Resolved;
use crate::types::{SynthesisRequest, Voice, WordTiming};

/// Mutable per-client knowledge shared by provider calls.
#[derive(Debug, Default)]
pub(crate) struct ProviderState {
    /// Kokoro roots whose `/dev/captioned_speech` route returned 404 (so later cues go straight to the
    /// OpenAI-compatible endpoint).
    pub kokoro_without_captions: Mutex<BTreeSet<String>>,
    /// Instances (name, endpoint URL) whose server refused or could not be reached, with the hint of that
    /// failure: their remaining cues fail at once instead of each waiting for a connection.
    pub unreachable: Mutex<BTreeMap<(String, String), Option<String>>>,
}

/// Everything a provider call needs besides the request.
pub(crate) struct Ctx<'a> {
    pub http: &'a HttpClients,
    pub env: &'a EnvSource,
    pub retry: RetryPolicy,
    pub state: &'a ProviderState,
    /// Extra environment variables removed from child processes ([`crate::TtsOptions::child_env_remove`]).
    pub child_env_remove: &'a [String],
}

/// What a provider returns before caching and post-processing.
#[derive(Debug)]
pub(crate) struct RawSynthesis {
    pub audio: AudioBuffer,
    pub words: Option<Vec<WordTiming>>,
    pub notes: Vec<String>,
    /// The provider-native format actually received (cache metadata).
    pub output_format: Option<String>,
}

/// Synthesize one request with its provider. Errors and notes are scrubbed of the instance's API key and
/// of URL userinfo (a last line of defence: provider paths scrub already).
pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let result = match req.provider.provider_type {
        ProviderType::Elevenlabs => elevenlabs::synthesize(ctx, req, resolved).await,
        ProviderType::Openai => openai::synthesize(ctx, req, resolved).await,
        ProviderType::Cartesia => cartesia::synthesize(ctx, req, resolved).await,
        ProviderType::Kokoro => kokoro::synthesize(ctx, req, resolved).await,
        ProviderType::Piper => piper::synthesize(ctx, req, resolved).await,
        ProviderType::MacosSay => macos_say::synthesize(ctx, req, resolved).await,
        ProviderType::Command => command::synthesize(ctx, req, resolved).await,
        ProviderType::Mock => Ok(mock_raw(&req.text, resolved)),
    };
    let secrets = keys::secrets(ctx.env, &req.provider);
    match result {
        Ok(mut raw) => {
            for note in &mut raw.notes {
                *note = keys::scrub_text(note, &secrets);
            }
            Ok(raw)
        }
        Err(error) => Err(keys::scrub_error(error, &secrets)),
    }
}

/// The mock provider's raw output (pure).
pub(crate) fn mock_raw(text: &str, resolved: &Resolved) -> RawSynthesis {
    let voice_id = resolved.voice_id.as_deref().unwrap_or("mock");
    let (audio, words) = mock::synthesize(text, voice_id, &resolved.settings);
    RawSynthesis {
        audio,
        words: Some(words),
        notes: Vec::new(),
        output_format: None,
    }
}

/// Voices offered by an instance (may call the provider's API). `base_dir` resolves relative piper paths
/// (`voices_dir`, a relative `PIPER_VOICES_DIR`) and is itself searched for piper models; without it those
/// locations are skipped. Voices and errors are scrubbed of the instance's key: a server that received it
/// could echo it back in a voice name, which would then reach logs and MCP results.
pub(crate) async fn list_voices(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    base_dir: Option<&Path>,
    base_label: &'static str,
) -> Result<Vec<Voice>, TtsError> {
    let result = match origin.provider_type {
        ProviderType::Elevenlabs => elevenlabs::list_voices(ctx, origin).await,
        ProviderType::Openai => openai::list_voices(ctx, origin).await,
        ProviderType::Cartesia => cartesia::list_voices(ctx, origin).await,
        ProviderType::Kokoro => kokoro::list_voices(ctx, origin).await,
        ProviderType::Piper => piper::list_voices(ctx.env, origin, base_dir, base_label),
        ProviderType::MacosSay => macos_say::list_voices(origin).await,
        ProviderType::Command => Ok(Vec::new()),
        ProviderType::Mock => Ok(vec![Voice {
            id: "mock".to_owned(),
            name: "Mock".to_owned(),
            language: None,
            gender: None,
            description: Some(
                "Deterministic synthetic voice (tone bursts); any voice_id works and sets the pitch".to_owned(),
            ),
        }]),
    };
    let secrets = keys::secrets(ctx.env, origin);
    match result {
        Ok(voices) if secrets.is_empty() => Ok(voices),
        Ok(voices) => {
            let scrub = |s: String| keys::scrub_text(&s, &secrets);
            Ok(voices
                .into_iter()
                .map(|v| Voice {
                    id: scrub(v.id),
                    name: scrub(v.name),
                    language: v.language.map(scrub),
                    gender: v.gender.map(scrub),
                    description: v.description.map(scrub),
                })
                .collect())
        }
        Err(e) => Err(keys::scrub_error(e, &secrets)),
    }
}

/// Cheap authenticated request proving the key works (or that a local server is reachable).
pub(crate) async fn check_credentials(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
) -> Result<(), TtsError> {
    let result = match origin.provider_type {
        ProviderType::Elevenlabs => elevenlabs::check_key(ctx, origin).await,
        ProviderType::Openai => openai::check_key(ctx, origin).await,
        ProviderType::Cartesia => cartesia::check_key(ctx, origin).await,
        ProviderType::Kokoro => kokoro::check_server(ctx, origin).await,
        ProviderType::Piper
        | ProviderType::MacosSay
        | ProviderType::Command
        | ProviderType::Mock => Ok(()),
    };
    result.map_err(|e| keys::scrub_error(e, &keys::secrets(ctx.env, origin)))
}

/// The base URL requests of `provider` go to (the instance `base_url`, else for kokoro `KOKORO_BASE_URL`,
/// else the built-in default), exactly as synthesis uses it; `None` for local programs (piper, macos_say,
/// command, mock). It may contain `user:password@`: use [`crate::redact_url`] before showing it.
pub fn endpoint_url(provider: &ProviderInstance, env: &EnvSource) -> Option<String> {
    match provider.provider_type {
        ProviderType::Elevenlabs => Some(elevenlabs::base_url(provider)),
        ProviderType::Openai => Some(openai::base_url(provider)),
        ProviderType::Cartesia => Some(cartesia::base_url(provider)),
        ProviderType::Kokoro => Some(kokoro::root_url(provider, env)),
        ProviderType::Piper
        | ProviderType::MacosSay
        | ProviderType::Command
        | ProviderType::Mock => None,
    }
}

/// True when synthesis sends the instance's key only if it is set (OpenAI-compatible servers, Kokoro).
pub(crate) fn key_is_optional(provider: &ProviderInstance) -> bool {
    match provider.provider_type {
        ProviderType::Openai => !openai::is_official(&openai::base_url(provider)),
        ProviderType::Kokoro => true,
        _ => false,
    }
}

/// The `settings` keys each provider reads (DESIGN.md §2.7). Every other key is ignored, with a
/// `setting_ignored` note ([`voice_notes`]). `x-`/`x_` keys are annotations and never noted. Piper with a
/// custom `command` and the `command` provider pass no settings to their program at all.
pub fn settings_keys(provider_type: ProviderType) -> &'static [&'static str] {
    match provider_type {
        ProviderType::Elevenlabs => &[
            "stability",
            "similarity_boost",
            "style",
            "use_speaker_boost",
            "speed",
            "voice_settings",
            "seed",
            "language_code",
            "apply_text_normalization",
            "apply_language_text_normalization",
            "pronunciation_dictionary_locators",
            "previous_text",
            "next_text",
            "use_pvc_as_ivc",
        ],
        ProviderType::Openai => &["instructions", "speed", "extra_body"],
        ProviderType::Cartesia => &[
            "speed",
            "volume",
            "emotion",
            "generation_config",
            "locale",
            "accent",
            "normalization",
            "pronunciation_dict_id",
        ],
        ProviderType::Kokoro => &[
            "speed",
            "lang_code",
            "volume_multiplier",
            "normalization_options",
            "extra_body",
        ],
        ProviderType::Piper => &[
            "speaker",
            "speaker_id",
            "length_scale",
            "speed",
            "noise_scale",
            "noise_w",
            "noise_w_scale",
            "sentence_silence",
        ],
        ProviderType::MacosSay => &["rate", "speed"],
        ProviderType::Command => &[],
        ProviderType::Mock => &["wpm", "speed"],
    }
}

/// `` `a`, `b` `` of `keys`.
fn quoted_list(keys: &[&str]) -> String {
    keys.iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_annotation(key: &str) -> bool {
    key.starts_with("x-") || key.starts_with("x_")
}

/// One `setting_ignored` note per settings key the provider does not read.
fn unread_settings_notes(provider: &ProviderInstance, resolved: &Resolved) -> Vec<String> {
    let t = provider.provider_type;
    let custom_piper = t == ProviderType::Piper
        && provider
            .command
            .as_deref()
            .is_some_and(|argv| !argv.is_empty());
    let read: &[&str] = if custom_piper { &[] } else { settings_keys(t) };
    let reads = if custom_piper {
        "a custom `command` gets no settings (use the template placeholders)".to_owned()
    } else if read.is_empty() {
        "settings are never passed to the program (use the template placeholders)".to_owned()
    } else {
        format!("it reads {}", quoted_list(read))
    };
    let mut notes = Vec::new();
    for (key, value) in &resolved.settings {
        if is_annotation(key) {
            continue;
        }
        if !read.contains(&key.as_str()) {
            notes.push(format!("{t} ignores `{key}`; {reads}"));
            continue;
        }
        // ElevenLabs' nested `voice_settings` object is filtered key by key.
        if t == ProviderType::Elevenlabs
            && key == "voice_settings"
            && let Value::Object(nested) = value
        {
            for inner in nested.keys().filter(|k| !is_annotation(k)) {
                if !elevenlabs::VOICE_SETTINGS_KEYS.contains(&inner.as_str()) {
                    notes.push(format!(
                        "{t} ignores `voice_settings.{inner}`; `voice_settings` reads {}",
                        quoted_list(&elevenlabs::VOICE_SETTINGS_KEYS)
                    ));
                }
            }
        }
    }
    notes
}

/// Informational notes about how `provider` will treat `voice` (settings a model ignores, fields never sent,
/// settings keys the provider does not read), for `setting_ignored` diagnostics at validation time. The same
/// notes are attached to every [`crate::SynthesisResult`].
pub fn voice_notes(provider: &ProviderInstance, voice: &VoiceIr) -> Vec<String> {
    let resolved = Resolved::new(provider, voice);
    static_notes(provider, &resolved)
}

pub(crate) fn static_notes(provider: &ProviderInstance, resolved: &Resolved) -> Vec<String> {
    let mut notes = match provider.provider_type {
        ProviderType::Elevenlabs => elevenlabs::build_body("", resolved).1,
        ProviderType::Openai => {
            let base = openai::base_url(provider);
            openai::build_body("", resolved, "pcm", openai::is_official(&base)).1
        }
        _ => Vec::new(),
    };
    for note in unread_settings_notes(provider, resolved) {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    notes
}

/// Decode encoded provider audio (mp3, wav, flac, …) off the async threads.
pub(crate) async fn decode_encoded(
    origin: &ProviderInstance,
    bytes: Vec<u8>,
    extension_hint: Option<&str>,
) -> Result<AudioBuffer, TtsError> {
    if bytes.is_empty() {
        return Err(TtsError::new(
            TtsErrorKind::Provider,
            origin,
            "the provider returned no audio data",
        ));
    }
    let hint = extension_hint.map(str::to_owned);
    let decoded = tokio::task::spawn_blocking(move || {
        soniccraft_dsp::decode::decode_bytes(bytes, hint.as_deref())
    })
    .await
    .map_err(|e| {
        TtsError::new(
            TtsErrorKind::Decode,
            origin,
            format!("the audio decoder failed: {e}"),
        )
    })?;
    decoded.map_err(|e| {
        TtsError::new(
            TtsErrorKind::Decode,
            origin,
            format!("cannot decode the audio returned by the provider: {e}"),
        )
    })
}

/// Raw headerless PCM → buffer, rejecting empty audio.
pub(crate) fn pcm_audio(
    origin: &ProviderInstance,
    bytes: &[u8],
    encoding: PcmEncoding,
    sample_rate: u32,
) -> Result<AudioBuffer, TtsError> {
    let frame_bytes = match encoding {
        PcmEncoding::S16le => 2,
        PcmEncoding::F32le => 4,
    };
    if bytes.len() < frame_bytes {
        return Err(TtsError::new(
            TtsErrorKind::Provider,
            origin,
            "the provider returned no audio data",
        ));
    }
    Ok(match encoding {
        PcmEncoding::S16le => soniccraft_dsp::decode::pcm_s16le_to_buffer(bytes, sample_rate, 1),
        PcmEncoding::F32le => soniccraft_dsp::decode::pcm_f32le_to_buffer(bytes, sample_rate, 1),
    })
}

/// Sample encodings of headerless mono PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PcmEncoding {
    S16le,
    F32le,
}

/// Decode base64 that may contain whitespace or lack padding.
pub(crate) fn decode_base64(origin: &ProviderInstance, text: &str) -> Result<Vec<u8>, TtsError> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD_PAD_INDIFFERENT;
    let compact: String = text.split_ascii_whitespace().collect();
    STANDARD_PAD_INDIFFERENT
        .decode(compact.as_bytes())
        .map_err(|e| {
            TtsError::new(
                TtsErrorKind::Decode,
                origin,
                format!("invalid base64 audio from the provider: {e}"),
            )
        })
}

/// A `missing voice_id` error for providers without a default voice.
pub(crate) fn missing_voice(origin: &ProviderInstance) -> TtsError {
    TtsError::new(
        TtsErrorKind::InvalidRequest,
        origin,
        format!(
            "{} requires a `voice_id`",
            crate::defaults::display_name(origin.provider_type)
        ),
    )
    .with_hint(format!(
        "set `voice_id` on the cue, track, speaker or defaults (`soniccraft voices {}` lists them)",
        origin.provider_type
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, voice};
    use serde_json::json;

    fn notes(t: ProviderType, settings: serde_json::Value) -> Vec<String> {
        voice_notes(&instance(t), &voice(t, Some("v"), None, None, settings))
    }

    #[test]
    fn unread_settings_get_a_note_naming_what_the_provider_reads() {
        assert_eq!(
            notes(
                ProviderType::MacosSay,
                json!({"voice": "Karen", "rate": 200})
            ),
            ["macos_say ignores `voice`; it reads `rate`, `speed`"]
        );
        let openai = notes(
            ProviderType::Openai,
            json!({"voice": "nova", "model": "tts-1-hd", "speed": 1.1}),
        );
        assert_eq!(
            openai,
            [
                "openai ignores `voice`; it reads `instructions`, `speed`, `extra_body`",
                "openai ignores `model`; it reads `instructions`, `speed`, `extra_body`"
            ]
        );
        assert!(notes(ProviderType::Mock, json!({"wpm": 150, "speed": 1.2})).is_empty());
        assert_eq!(
            notes(ProviderType::Mock, json!({"foo": "bar"})),
            ["mock ignores `foo`; it reads `wpm`, `speed`"]
        );
        let command = notes(ProviderType::Command, json!({"speed": 2}));
        assert_eq!(command.len(), 1);
        assert!(command[0].starts_with("command ignores `speed`; settings are never passed"));
        // Annotations are never noted.
        assert!(
            notes(
                ProviderType::MacosSay,
                json!({"x-why": "draft", "x_note": 1})
            )
            .is_empty()
        );
        // Nested ElevenLabs voice settings are checked key by key.
        let el = notes(
            ProviderType::Elevenlabs,
            json!({"voice_settings": {"stability": 0.5, "pitch": 2}, "stability": 0.4}),
        );
        assert_eq!(el.len(), 1, "{el:?}");
        assert!(el[0].starts_with("elevenlabs ignores `voice_settings.pitch`"));
        // Every documented key of every provider is read.
        for &t in soniccraft_core::providers::all() {
            let all: serde_json::Map<String, Value> = settings_keys(t)
                .iter()
                .map(|k| ((*k).to_owned(), json!(1)))
                .collect();
            let unread = unread_settings_notes(
                &instance(t),
                &Resolved::new(
                    &instance(t),
                    &voice(t, Some("v"), None, None, Value::Object(all)),
                ),
            );
            assert!(unread.is_empty(), "{t}: {unread:?}");
        }
    }

    #[test]
    fn piper_with_a_custom_command_reads_no_settings() {
        let mut p = instance(ProviderType::Piper);
        p.command = Some(vec!["my-piper".into(), "{voice_id}".into()]);
        let n = voice_notes(
            &p,
            &voice(
                ProviderType::Piper,
                Some("v"),
                None,
                None,
                json!({"length_scale": 1.2}),
            ),
        );
        assert_eq!(n.len(), 1);
        assert!(n[0].contains("custom `command` gets no settings"), "{n:?}");
    }

    #[test]
    fn endpoint_urls() {
        let env = EnvSource::fixed([("KOKORO_BASE_URL", "http://gpu-box:8880/v1")]);
        assert_eq!(
            endpoint_url(&instance(ProviderType::Elevenlabs), &env).as_deref(),
            Some("https://api.elevenlabs.io")
        );
        assert_eq!(
            endpoint_url(&instance(ProviderType::Openai), &env).as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            endpoint_url(&instance(ProviderType::Kokoro), &env).as_deref(),
            Some("http://gpu-box:8880")
        );
        assert_eq!(endpoint_url(&instance(ProviderType::Piper), &env), None);
        let mut local = instance(ProviderType::Openai);
        local.base_url = Some("http://localhost:8880/v1".into());
        assert!(key_is_optional(&local));
        assert!(!key_is_optional(&instance(ProviderType::Openai)));
        assert!(key_is_optional(&instance(ProviderType::Kokoro)));
        assert!(!key_is_optional(&instance(ProviderType::Cartesia)));
    }
}
