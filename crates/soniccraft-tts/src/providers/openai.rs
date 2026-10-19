//! OpenAI and OpenAI-compatible servers: `POST {base}/audio/speech` (`Authorization: Bearer`) with
//! `response_format: "pcm"` (headerless 24 kHz s16le mono).
//!
//! Settings: `instructions` (not for `tts-1*`), `speed` (rounded to 2 decimals), and `extra_body` (an object of
//! extra request fields, sent only to non-OpenAI servers such as Kokoro-FastAPI, Speaches or LocalAI). Nothing
//! else is ever sent to api.openai.com, whose schema rejects unknown fields. An instance `output_format`
//! selects another `response_format` (wav, mp3, flac, aac, opus), decoded by the DSP crate.

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use serde_json::{Map, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType};

use super::{Ctx, PcmEncoding, RawSynthesis, decode_encoded, pcm_audio};
use crate::defaults;
use crate::error::{TtsError, TtsErrorKind};
use crate::http::{self, Deadline, request_timeout, with_retries};
use crate::settings::{self, Resolved, get_f64, get_object, get_str};
use crate::types::{SynthesisRequest, Voice};
use crate::{hosts, keys};

/// Host of the official API.
const OFFICIAL_HOST: &str = "api.openai.com";
/// Sample rate of `response_format: "pcm"`.
const PCM_SAMPLE_RATE: u32 = 24_000;
/// Voices of `gpt-4o-mini-tts` (also the fallback list).
const GPT_4O_MINI_TTS_VOICES: [&str; 13] = [
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
    "marin", "cedar",
];
/// Voices of `tts-1` and `tts-1-hd`.
const TTS_1_VOICES: [&str; 9] = [
    "alloy", "ash", "coral", "echo", "fable", "onyx", "nova", "sage", "shimmer",
];
const RESPONSE_FORMATS: [&str; 6] = ["pcm", "wav", "mp3", "flac", "aac", "opus"];

/// The base URL (no trailing slash), e.g. `https://api.openai.com/v1`.
pub(crate) fn base_url(origin: &ProviderInstance) -> String {
    http::trim_base(
        settings::non_empty(origin.base_url.as_deref())
            .or(defaults::default_base_url(ProviderType::Openai))
            .unwrap_or_default(),
    )
}

/// True when `base` points at OpenAI itself (strict schema, key required).
pub(crate) fn is_official(base: &str) -> bool {
    reqwest::Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case(OFFICIAL_HOST)))
        .unwrap_or(false)
}

/// Request body plus informational notes.
pub(crate) fn build_body(
    text: &str,
    resolved: &Resolved,
    response_format: &str,
    official: bool,
) -> (Value, Vec<String>) {
    let s = &resolved.settings;
    let model = resolved
        .model
        .as_deref()
        .or(defaults::default_model(ProviderType::Openai))
        .unwrap_or_default();
    let voice = resolved
        .voice_id
        .as_deref()
        .or(defaults::default_voice(ProviderType::Openai))
        .unwrap_or_default();
    let mut notes = Vec::new();
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.to_owned()));
    body.insert("input".into(), Value::String(text.to_owned()));
    // Custom OpenAI voices are objects: {"id": "voice_…"}.
    let voice_value = if official && voice.starts_with("voice_") {
        serde_json::json!({ "id": voice })
    } else {
        Value::String(voice.to_owned())
    };
    body.insert("voice".into(), voice_value);
    body.insert(
        "response_format".into(),
        Value::String(response_format.to_owned()),
    );
    if let Some(instructions) = get_str(s, "instructions") {
        if official && model.starts_with("tts-1") {
            notes.push(format!(
                "`{model}` does not support `instructions`; ignoring them"
            ));
        } else {
            body.insert("instructions".into(), Value::String(instructions));
        }
    }
    if let Some(speed) = get_f64(s, "speed").filter(|v| *v > 0.0) {
        // More than 16 decimals is a 400 (`decimal_max_decimal_places_exceeded`); 2 is plenty.
        if let Some(v) = settings::number((speed * 100.0).round() / 100.0) {
            body.insert("speed".into(), v);
        }
    }
    if let Some(extra) = get_object(s, "extra_body") {
        if official {
            notes.push(
                "`extra_body` is never sent to api.openai.com (its schema rejects unknown fields)"
                    .to_owned(),
            );
        } else {
            for (k, v) in extra {
                body.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    (Value::Object(body), notes)
}

/// The `response_format` to request.
fn response_format(origin: &ProviderInstance) -> String {
    settings::non_empty(origin.output_format.as_deref())
        .unwrap_or("pcm")
        .to_ascii_lowercase()
}

/// Bearer header (and the key, to scrub from messages): required for api.openai.com, optional (sent when
/// set) for compatible servers; refused for hosts the key-sending policy does not allow.
fn auth_header(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    official: bool,
    url: &str,
) -> Result<(Option<HeaderValue>, Vec<String>), TtsError> {
    let key = if official {
        Some(keys::require(ctx.env, origin, url)?)
    } else {
        keys::optional(ctx.env, origin, url)?
    };
    let secrets = key.as_deref().map(keys::secrets_of).unwrap_or_default();
    let header = key
        .map(|k| http::secret_header(origin, &format!("Bearer {k}")))
        .transpose()?;
    Ok((header, secrets))
}

pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    let base = base_url(origin);
    let official = is_official(&base);
    let format = response_format(origin);
    if official && !RESPONSE_FORMATS.contains(&format.as_str()) {
        return Err(TtsError::new(
            TtsErrorKind::InvalidRequest,
            origin,
            format!("unsupported OpenAI response format `{format}`"),
        )
        .with_hint("set `output_format` to one of pcm (default), wav, mp3, flac, aac, opus"));
    }
    let url = format!("{base}/audio/speech");
    let (auth, secrets) = auth_header(ctx, origin, official, &url)?;
    let (body, notes) = build_body(&req.text, resolved, &format, official);
    let client = ctx.http.get(origin, &url, request_timeout(origin))?;
    let url_ref = url.as_str();
    let bytes = with_retries(
        origin,
        ctx.retry,
        Deadline::for_instance(origin),
        &secrets,
        |_, left| {
            let mut request = client
                .post(url_ref)
                .header(ACCEPT, "*/*")
                .json(&body)
                .timeout(left);
            if let Some(auth) = &auth {
                request = request.header(AUTHORIZATION, auth.clone());
            }
            async move {
                let response = request
                    .send()
                    .await
                    .map_err(|e| http::transport_error(origin, url_ref, &e))?;
                if !response.status().is_success() {
                    // OpenAI sometimes labels JSON errors text/plain: the parser ignores Content-Type.
                    return Err(not_found_hint(
                        http::response_error(origin, response).await,
                        url_ref,
                    ));
                }
                http::read_body(origin, url_ref, response, http::MAX_RESPONSE_BYTES).await
            }
        },
    )
    .await?;
    let audio = if format == "pcm" {
        pcm_audio(origin, &bytes, PcmEncoding::S16le, PCM_SAMPLE_RATE)?
    } else {
        let hint = RESPONSE_FORMATS.iter().copied().find(|f| *f == format);
        decode_encoded(origin, bytes, hint).await?
    };
    Ok(RawSynthesis {
        audio,
        words: None,
        notes,
        output_format: Some(format),
    })
}

/// A 404 from an OpenAI-compatible server usually means a `base_url` without `/v1`.
fn not_found_hint(mut error: TtsError, url: &str) -> TtsError {
    if error.status == Some(404)
        && error.code.is_none()
        && !error.message.to_ascii_lowercase().contains("voice")
    {
        error.hint = Some(format!(
            "{} does not exist; OpenAI-compatible base URLs usually end in /v1 (e.g. http://localhost:8880/v1)",
            hosts::redact_url(url)
        ));
    }
    error
}

/// Voices: OpenAI publishes no list endpoint, so the documented lists are returned; compatible servers are
/// asked via `GET {base}/audio/voices` first (Kokoro-FastAPI, Speaches), falling back to the static list.
pub(crate) async fn list_voices(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
) -> Result<Vec<Voice>, TtsError> {
    let base = base_url(origin);
    if !is_official(&base) {
        let url = format!("{base}/audio/voices");
        if let Ok(body) = get_json(ctx, origin, &url, false).await {
            let voices = super::kokoro::parse_voice_list(&body);
            if !voices.is_empty() {
                return Ok(voices);
            }
        }
    }
    let model = settings::non_empty(origin.model.as_deref()).unwrap_or("gpt-4o-mini-tts");
    let names: &[&str] = if model.starts_with("tts-1") {
        &TTS_1_VOICES
    } else {
        &GPT_4O_MINI_TTS_VOICES
    };
    Ok(names
        .iter()
        .map(|n| Voice {
            id: (*n).to_owned(),
            name: (*n).to_owned(),
            language: None,
            gender: None,
            description: matches!(*n, "marin" | "cedar")
                .then(|| "recommended for best quality".to_owned()),
        })
        .collect())
}

async fn get_json(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    url: &str,
    official: bool,
) -> Result<Value, TtsError> {
    let (auth, secrets) = auth_header(ctx, origin, official, url)?;
    let client = ctx.http.get(origin, url, request_timeout(origin))?;
    with_retries(
        origin,
        ctx.retry,
        Deadline::for_instance(origin),
        &secrets,
        |_, left| {
            let mut request = client.get(url).timeout(left);
            if let Some(auth) = &auth {
                request = request.header(AUTHORIZATION, auth.clone());
            }
            async move {
                let response = request
                    .send()
                    .await
                    .map_err(|e| http::transport_error(origin, url, &e))?;
                if !response.status().is_success() {
                    return Err(http::response_error(origin, response).await);
                }
                http::read_json(origin, url, response).await
            }
        },
    )
    .await
}

/// `GET {base}/models`.
pub(crate) async fn check_key(ctx: &Ctx<'_>, origin: &ProviderInstance) -> Result<(), TtsError> {
    let base = base_url(origin);
    let official = is_official(&base);
    get_json(ctx, origin, &format!("{base}/models"), official)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::resolved;
    use serde_json::json;

    #[test]
    fn official_body_has_only_documented_fields() {
        let r = resolved(
            ProviderType::Openai,
            Some("coral"),
            None,
            None,
            json!({"instructions": "Speak warmly.", "speed": 1.23456, "stability": 0.5, "extra_body": {"stream": false}}),
        );
        let (body, notes) = build_body("Hello", &r, "pcm", true);
        assert_eq!(
            body,
            json!({"model": "gpt-4o-mini-tts", "input": "Hello", "voice": "coral", "response_format": "pcm",
                   "instructions": "Speak warmly.", "speed": 1.23})
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn tts1_drops_instructions_and_custom_voices_are_objects() {
        let r = resolved(
            ProviderType::Openai,
            Some("voice_123"),
            Some("tts-1-hd"),
            None,
            json!({"instructions": "x"}),
        );
        let (body, notes) = build_body("Hi", &r, "pcm", true);
        assert!(body.get("instructions").is_none());
        assert_eq!(body["voice"], json!({"id": "voice_123"}));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn compatible_servers_get_extra_body_and_plain_voices() {
        let r = resolved(
            ProviderType::Openai,
            Some("voice_x"),
            Some("kokoro"),
            None,
            json!({"extra_body": {"lang_code": "a", "stream": false, "model": "ignored"}}),
        );
        let (body, notes) = build_body("Hi", &r, "pcm", false);
        assert!(notes.is_empty());
        assert_eq!(body["voice"], "voice_x");
        assert_eq!(body["lang_code"], "a");
        assert_eq!(body["stream"], false);
        assert_eq!(
            body["model"], "kokoro",
            "documented fields win over extra_body"
        );
    }

    #[test]
    fn official_detection() {
        assert!(is_official("https://api.openai.com/v1"));
        assert!(is_official("https://API.OPENAI.COM/v1/"));
        assert!(!is_official("http://localhost:8880/v1"));
        assert!(!is_official("https://api.openai.com.evil.example/v1"));
    }
}
