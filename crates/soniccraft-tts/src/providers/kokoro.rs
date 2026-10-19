//! Kokoro via Kokoro-FastAPI: `POST {root}/dev/captioned_speech` (no `/v1` prefix; `stream: false`,
//! `response_format: "pcm"`, `return_timestamps: true`) returning base64 audio and word timestamps. When that
//! route does not exist (404), the client falls back to `POST {root}/v1/audio/speech` and remembers the
//! server lacks captions.
//!
//! The root is the instance `base_url`, else `KOKORO_BASE_URL`, else `http://localhost:8880`; a trailing
//! `/v1` is accepted and stripped. Settings: `speed`, `lang_code` (one of a b e f h i j p z),
//! `volume_multiplier`, `normalization_options` (object) and `extra_body` (object, merged into the body).
//! An optional key is sent as a Bearer token when the instance's key variable (default `KOKORO_API_KEY`) is set.
//! Raw PCM is always requested, so an instance `output_format` is ignored.

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType};

use super::{Ctx, PcmEncoding, RawSynthesis, decode_base64, decode_encoded, pcm_audio};
use crate::defaults::{self, KOKORO_BASE_URL_ENV, KOKORO_DEFAULT_ROOT};
use crate::env::EnvSource;
use crate::error::{TtsError, TtsErrorKind};
use crate::http::{self, Deadline, request_timeout, with_retries};
use crate::settings::{self, Resolved, get_f64, get_object, get_str};
use crate::types::{SynthesisRequest, Voice, WordTiming};
use crate::{hosts, keys};

/// Sample rate of Kokoro's raw PCM.
const PCM_SAMPLE_RATE: u32 = 24_000;

/// The server root (no trailing slash, no `/v1`).
pub(crate) fn root_url(origin: &ProviderInstance, env: &EnvSource) -> String {
    let base = settings::non_empty(origin.base_url.as_deref())
        .map(str::to_owned)
        .or_else(|| env.get(KOKORO_BASE_URL_ENV))
        .unwrap_or_else(|| KOKORO_DEFAULT_ROOT.to_owned());
    normalize_root(&base)
}

/// Strip trailing slashes and a trailing `/v1`.
pub(crate) fn normalize_root(base: &str) -> String {
    let trimmed = http::trim_base(base);
    match trimmed.strip_suffix("/v1") {
        Some(root) => http::trim_base(root),
        None => trimmed,
    }
}

/// Request body for either endpoint.
pub(crate) fn build_body(text: &str, resolved: &Resolved, captioned: bool) -> Value {
    let s = &resolved.settings;
    let mut body = Map::new();
    let model = resolved
        .model
        .as_deref()
        .or(defaults::default_model(ProviderType::Kokoro))
        .unwrap_or_default();
    let voice = resolved
        .voice_id
        .as_deref()
        .or(defaults::default_voice(ProviderType::Kokoro))
        .unwrap_or_default();
    body.insert("model".into(), Value::String(model.to_owned()));
    body.insert("input".into(), Value::String(text.to_owned()));
    body.insert("voice".into(), Value::String(voice.to_owned()));
    body.insert("response_format".into(), Value::String("pcm".to_owned()));
    // Complete bodies only: with streaming, errors could arrive after a 200.
    body.insert("stream".into(), Value::Bool(false));
    if captioned {
        body.insert("return_timestamps".into(), Value::Bool(true));
    }
    if let Some(speed) = get_f64(s, "speed")
        .filter(|v| *v > 0.0)
        .and_then(settings::number)
    {
        body.insert("speed".into(), speed);
    }
    if let Some(lang) = get_str(s, "lang_code") {
        body.insert("lang_code".into(), Value::String(lang));
    }
    if let Some(volume) = get_f64(s, "volume_multiplier").and_then(settings::number) {
        body.insert("volume_multiplier".into(), volume);
    }
    if let Some(options) = get_object(s, "normalization_options") {
        body.insert(
            "normalization_options".into(),
            Value::Object(options.clone()),
        );
    }
    if let Some(extra) = get_object(s, "extra_body") {
        for (k, v) in extra {
            body.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    Value::Object(body)
}

#[derive(Debug, Deserialize)]
struct CaptionedResponse {
    audio: String,
    #[serde(default)]
    audio_format: Option<String>,
    #[serde(default)]
    timestamps: Option<Vec<CaptionedWord>>,
}

#[derive(Debug, Deserialize)]
struct CaptionedWord {
    #[serde(default)]
    word: String,
    #[serde(default)]
    start_time: Option<f64>,
    #[serde(default)]
    end_time: Option<f64>,
}

/// Merge punctuation-only tokens into the previous word (leading ones into the next word).
pub(crate) fn fold_punctuation(tokens: Vec<(String, f64, f64)>) -> Vec<WordTiming> {
    let mut words: Vec<WordTiming> = Vec::new();
    let mut leading = String::new();
    for (token, start, end) in tokens {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if !crate::text::is_spoken_token(token) {
            match words.last_mut() {
                Some(last) => last.text.push_str(token),
                None => leading.push_str(token),
            }
            continue;
        }
        let text = format!("{}{token}", std::mem::take(&mut leading));
        words.push(WordTiming {
            text,
            start_secs: start,
            end_secs: end.max(start),
        });
    }
    words
}

/// The extension of a captioned response's `audio_format` MIME type (`None` = raw PCM).
fn encoded_extension(audio_format: Option<&str>) -> Option<&'static str> {
    let mime = audio_format
        .unwrap_or("audio/pcm")
        .trim()
        .to_ascii_lowercase();
    let subtype = mime.rsplit('/').next().unwrap_or("pcm");
    match subtype {
        "pcm" | "l16" | "raw" => None,
        "mpeg" | "mp3" => Some("mp3"),
        "wav" | "wave" | "x-wav" => Some("wav"),
        "flac" => Some("flac"),
        "opus" => Some("opus"),
        "ogg" => Some("ogg"),
        "aac" => Some("aac"),
        _ => Some("bin"),
    }
}

/// A parsed `/dev/captioned_speech` response.
#[derive(Debug)]
struct Captioned {
    /// Audio bytes in the response's `audio_format`.
    audio: Vec<u8>,
    /// Decoder hint for encoded audio; `None` = raw s16le PCM.
    encoded: Option<&'static str>,
    words: Option<Vec<WordTiming>>,
}

/// Parse `/dev/captioned_speech`'s JSON into audio bytes, their encoding and word timings.
fn parse_captioned(origin: &ProviderInstance, body: &[u8]) -> Result<Captioned, TtsError> {
    let parsed: CaptionedResponse = serde_json::from_slice(body).map_err(|e| {
        TtsError::new(
            TtsErrorKind::Provider,
            origin,
            format!("unexpected Kokoro captioned_speech response: {e}"),
        )
    })?;
    let audio = decode_base64(origin, &parsed.audio)?;
    let tokens: Vec<(String, f64, f64)> = parsed
        .timestamps
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| Some((w.word, w.start_time?, w.end_time?)))
        .collect();
    let words = fold_punctuation(tokens);
    Ok(Captioned {
        audio,
        encoded: encoded_extension(parsed.audio_format.as_deref()),
        words: (!words.is_empty()).then_some(words),
    })
}

/// The optional Bearer header for requests to `url` (refused when a key is set but `url`'s host may not
/// receive it), and the key to scrub from messages.
fn auth(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    url: &str,
) -> Result<(Option<HeaderValue>, Vec<String>), TtsError> {
    let key = keys::optional(ctx.env, origin, url)?;
    let secrets = key.as_deref().map(keys::secrets_of).unwrap_or_default();
    let header = key
        .map(|k| http::secret_header(origin, &format!("Bearer {k}")))
        .transpose()?;
    Ok((header, secrets))
}

/// True when a 404 means "this route does not exist" (FastAPI's default body) rather than an app error.
fn is_missing_route(error: &TtsError) -> bool {
    error.status == Some(404)
        && error.code.is_none()
        && matches!(
            error.message.trim().to_ascii_lowercase().as_str(),
            "not found" | "404 not found" | ""
        )
}

async fn post(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    url: &str,
    body: &Value,
    deadline: Deadline,
) -> Result<Vec<u8>, TtsError> {
    let (auth, secrets) = auth(ctx, origin, url)?;
    let client = ctx.http.get(origin, url, request_timeout(origin))?;
    with_retries(origin, ctx.retry, deadline, &secrets, |_, left| {
        let mut request = client
            .post(url)
            .header(ACCEPT, "*/*")
            .json(body)
            .timeout(left);
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
            http::read_body(origin, url, response, http::MAX_RESPONSE_BYTES).await
        }
    })
    .await
}

pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    let root = root_url(origin, ctx.env);
    // One deadline for the cue, the fallback request included.
    let deadline = Deadline::for_instance(origin);
    let captions_known_missing = ctx
        .state
        .kokoro_without_captions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&root);
    if !captions_known_missing {
        let url = format!("{root}/dev/captioned_speech");
        match post(
            ctx,
            origin,
            &url,
            &build_body(&req.text, resolved, true),
            deadline,
        )
        .await
        {
            Ok(bytes) => {
                let Captioned {
                    audio,
                    encoded,
                    words,
                } = parse_captioned(origin, &bytes)?;
                let audio = match encoded {
                    None => pcm_audio(origin, &audio, PcmEncoding::S16le, PCM_SAMPLE_RATE)?,
                    Some(ext) => decode_encoded(origin, audio, Some(ext)).await?,
                };
                return Ok(RawSynthesis {
                    audio,
                    words,
                    notes: Vec::new(),
                    output_format: Some("pcm".to_owned()),
                });
            }
            Err(e) if is_missing_route(&e) => {
                tracing::debug!(
                    root = %hosts::redact_url(&root),
                    "Kokoro server has no /dev/captioned_speech; using /v1/audio/speech"
                );
                ctx.state
                    .kokoro_without_captions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(root.clone());
            }
            Err(e) => return Err(e),
        }
    }
    let url = format!("{root}/v1/audio/speech");
    let bytes = post(
        ctx,
        origin,
        &url,
        &build_body(&req.text, resolved, false),
        deadline,
    )
    .await?;
    let audio = pcm_audio(origin, &bytes, PcmEncoding::S16le, PCM_SAMPLE_RATE)?;
    Ok(RawSynthesis {
        audio,
        words: None,
        notes: Vec::new(),
        output_format: Some("pcm".to_owned()),
    })
}

/// Language and gender from a Kokoro voice name (`af_heart` → en-US, female).
fn describe_voice_name(id: &str) -> (Option<String>, Option<String>) {
    let mut chars = id.chars();
    let (Some(lang), Some(gender), Some('_')) = (chars.next(), chars.next(), chars.next()) else {
        return (None, None);
    };
    let language = match lang {
        'a' => Some("en-US"),
        'b' => Some("en-GB"),
        'e' => Some("es"),
        'f' => Some("fr"),
        'h' => Some("hi"),
        'i' => Some("it"),
        'j' => Some("ja"),
        'p' => Some("pt-BR"),
        'z' => Some("zh"),
        _ => None,
    };
    let gender = match gender {
        'f' => Some("female"),
        'm' => Some("male"),
        _ => None,
    };
    (language.map(str::to_owned), gender.map(str::to_owned))
}

/// Parse a voice list: `{"voices": [{"id", "name"}, …], "default_voice"}`, the legacy `{"voices": ["…"]}`,
/// or a bare array of names.
pub(crate) fn parse_voice_list(body: &Value) -> Vec<Voice> {
    let items = match body {
        Value::Array(items) => items.as_slice(),
        Value::Object(obj) => obj
            .get("voices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
        _ => &[],
    };
    let default_voice = body.get("default_voice").and_then(Value::as_str);
    items
        .iter()
        .filter_map(|item| {
            let (id, name) = match item {
                Value::String(s) => (s.clone(), s.clone()),
                Value::Object(o) => {
                    let id = o
                        .get("id")
                        .or_else(|| o.get("name"))
                        .and_then(Value::as_str)?
                        .to_owned();
                    let name = o
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned();
                    (id, name)
                }
                _ => return None,
            };
            let (language, gender) = describe_voice_name(&id);
            let description =
                (default_voice == Some(id.as_str())).then(|| "server default voice".to_owned());
            Some(Voice {
                id,
                name,
                language,
                gender,
                description,
            })
        })
        .collect()
}

pub(crate) async fn list_voices(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
) -> Result<Vec<Voice>, TtsError> {
    let url = format!("{}/v1/audio/voices", root_url(origin, ctx.env));
    let body = get_json(ctx, origin, &url).await?;
    Ok(parse_voice_list(&body))
}

async fn get_json(ctx: &Ctx<'_>, origin: &ProviderInstance, url: &str) -> Result<Value, TtsError> {
    let (auth, secrets) = auth(ctx, origin, url)?;
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

/// `GET {root}/health` (Kokoro-FastAPI's liveness probe).
pub(crate) async fn check_server(ctx: &Ctx<'_>, origin: &ProviderInstance) -> Result<(), TtsError> {
    let url = format!("{}/health", root_url(origin, ctx.env));
    get_json(ctx, origin, &url).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, resolved};
    use serde_json::json;

    #[test]
    fn root_resolution() {
        let mut o = instance(ProviderType::Kokoro);
        let env = EnvSource::fixed([("KOKORO_BASE_URL", "http://gpu-box:8880/v1/")]);
        assert_eq!(
            root_url(&o, &EnvSource::fixed([("X", "1")])),
            "http://localhost:8880"
        );
        assert_eq!(root_url(&o, &env), "http://gpu-box:8880");
        o.base_url = Some("http://127.0.0.1:9000/".into());
        assert_eq!(root_url(&o, &env), "http://127.0.0.1:9000");
    }

    #[test]
    fn bodies() {
        let r = resolved(
            ProviderType::Kokoro,
            None,
            None,
            None,
            json!({"speed": 1.1, "lang_code": "b", "extra_body": {"volume_multiplier": 2, "stream": true}}),
        );
        let captioned = build_body("Hi", &r, true);
        assert_eq!(
            captioned,
            json!({"model": "kokoro", "input": "Hi", "voice": "af_heart", "response_format": "pcm", "stream": false,
                   "return_timestamps": true, "speed": 1.1, "lang_code": "b", "volume_multiplier": 2})
        );
        assert!(
            build_body("Hi", &r, false)
                .get("return_timestamps")
                .is_none()
        );
    }

    #[test]
    fn punctuation_folds_into_neighbours() {
        let words = fold_punctuation(vec![
            ("¿".into(), 0.0, 0.0),
            ("Qué".into(), 0.1, 0.3),
            ("?".into(), 0.3, 0.3),
            ("Hello".into(), 0.4, 0.6),
            (",".into(), 0.6, 0.6),
            ("world".into(), 0.7, 0.9),
            ("!".into(), 0.9, 1.0),
            ("".into(), 1.0, 1.0),
        ]);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["¿Qué?", "Hello,", "world!"]);
        assert_eq!((words[2].start_secs, words[2].end_secs), (0.7, 0.9));
    }

    #[test]
    fn captioned_response() {
        use base64::Engine as _;
        let o = instance(ProviderType::Kokoro);
        let body = json!({
            "audio": base64::engine::general_purpose::STANDARD.encode([0u8, 1, 2, 3]),
            "audio_format": "audio/pcm",
            "timestamps": [{"word": "Hello", "start_time": 0.275, "end_time": 0.5}, {"word": "!", "start_time": 0.5, "end_time": 0.55},
                           {"word": "broken", "start_time": null, "end_time": 1.0}]
        });
        let Captioned {
            audio,
            encoded,
            words,
        } = parse_captioned(&o, body.to_string().as_bytes()).unwrap();
        assert_eq!(audio, [0, 1, 2, 3]);
        assert_eq!(encoded, None);
        let words = words.unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "Hello!");
        assert_eq!(encoded_extension(Some("audio/mpeg")), Some("mp3"));
        assert_eq!(encoded_extension(Some("audio/wav")), Some("wav"));
        let none = json!({"audio": "", "timestamps": null});
        assert_eq!(
            parse_captioned(&o, none.to_string().as_bytes())
                .unwrap()
                .words,
            None
        );
    }

    #[test]
    fn voice_lists_in_both_shapes() {
        let modern = parse_voice_list(
            &json!({"voices": [{"id": "af_bella", "name": "af_bella", "overall_grade": "A-"},
                                                           {"id": "bm_george", "name": "George"}], "default_voice": "af_bella"}),
        );
        assert_eq!(modern.len(), 2);
        assert_eq!(modern[0].language.as_deref(), Some("en-US"));
        assert_eq!(modern[0].gender.as_deref(), Some("female"));
        assert_eq!(
            modern[0].description.as_deref(),
            Some("server default voice")
        );
        assert_eq!(modern[1].name, "George");
        assert_eq!(modern[1].language.as_deref(), Some("en-GB"));
        let legacy = parse_voice_list(&json!({"voices": ["af_alloy", "zf_xiaoxiao"]}));
        assert_eq!(
            legacy.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["af_alloy", "zf_xiaoxiao"]
        );
        assert_eq!(legacy[1].language.as_deref(), Some("zh"));
        let bare = parse_voice_list(&json!(["am_adam"]));
        assert_eq!(bare[0].gender.as_deref(), Some("male"));
        assert!(parse_voice_list(&json!({"nope": 1})).is_empty());
    }

    #[test]
    fn missing_route_detection() {
        let o = instance(ProviderType::Kokoro);
        let route = http::status_error(
            &o,
            404,
            &http::parse_error_body(r#"{"detail":"Not Found"}"#),
            None,
        );
        assert!(is_missing_route(&route));
        let voice = http::status_error(
            &o,
            404,
            &http::parse_error_body(
                r#"{"detail":{"error":"voice_not_found","message":"Voice x not found"}}"#,
            ),
            None,
        );
        assert!(!is_missing_route(&voice));
    }
}
