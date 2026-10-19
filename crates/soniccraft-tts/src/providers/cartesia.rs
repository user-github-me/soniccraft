//! Cartesia: `POST {base}/tts/sse` (`Authorization: Bearer`, `Cartesia-Version: 2026-08-14`) streaming raw
//! `pcm_f32le` at 48 kHz with word timestamps (`add_timestamps: true`).
//!
//! The stream is dispatched on each frame's JSON `type` (`chunk`, `timestamps`, `done`, `error`); `event:`
//! lines are ignored, word-timestamp batches are concatenated, and an `error` event inside a 200 stream is a
//! failure. A stream that ends before `done` is a (retryable) network error. The whole stream must arrive
//! within the cue's `timeout_ms` (a server trickling keep-alives cannot stall it), stay under
//! [`http::MAX_STREAM_BYTES`] with lines of at most [`crate::sse::MAX_LINE_BYTES`], and carry at most
//! [`http::MAX_RESPONSE_BYTES`] of audio.
//!
//! Settings: `speed`, `volume`, `emotion` (→ `generation_config`, merged over a `generation_config` object),
//! `locale` (sent instead of `language`), and the pass-through fields `accent`, `normalization` and
//! `pronunciation_dict_id`. An instance `output_format` of `pcm_f32le` or `pcm_s16le`, optionally with a rate
//! suffix (`pcm_s16le_24000`), selects the stream encoding.

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use serde_json::{Map, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType};

use super::{Ctx, PcmEncoding, RawSynthesis, decode_base64, missing_voice, pcm_audio};
use crate::defaults::{self, CARTESIA_API_VERSION};
use crate::error::{TtsError, TtsErrorKind};
use crate::http::{self, Deadline, request_timeout, with_retries};
use crate::keys;
use crate::settings::{self, Resolved, get_object, get_str};
use crate::sse::SseParser;
use crate::types::{SynthesisRequest, Voice, WordTiming};

const VERSION_HEADER: &str = "cartesia-version";
const PASSTHROUGH_KEYS: [&str; 3] = ["accent", "normalization", "pronunciation_dict_id"];
const GENERATION_KEYS: [&str; 3] = ["speed", "volume", "emotion"];
const SAMPLE_RATES: [u32; 6] = [8_000, 16_000, 22_050, 24_000, 44_100, 48_000];
const MAX_VOICE_PAGES: usize = 50;

/// Raw stream encoding and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CartesiaFormat {
    pub encoding: PcmEncoding,
    pub sample_rate: u32,
}

impl CartesiaFormat {
    /// Canonical name, e.g. `pcm_f32le_48000` (cache metadata and keys).
    pub(crate) fn name(&self) -> String {
        format!("{}_{}", self.encoding_name(), self.sample_rate)
    }

    fn encoding_name(&self) -> &'static str {
        match self.encoding {
            PcmEncoding::F32le => "pcm_f32le",
            PcmEncoding::S16le => "pcm_s16le",
        }
    }
}

/// Parse an instance `output_format` (`None` → `pcm_f32le` at 48 kHz).
pub(crate) fn parse_output_format(format: Option<&str>) -> Result<CartesiaFormat, String> {
    let Some(raw) = settings::non_empty(format) else {
        return Ok(CartesiaFormat {
            encoding: PcmEncoding::F32le,
            sample_rate: 48_000,
        });
    };
    let lower = raw.to_ascii_lowercase();
    let (encoding, rest) = if let Some(rest) = lower.strip_prefix("pcm_f32le") {
        (PcmEncoding::F32le, rest)
    } else if let Some(rest) = lower.strip_prefix("pcm_s16le") {
        (PcmEncoding::S16le, rest)
    } else {
        return Err(format!(
            "unsupported Cartesia output_format `{raw}`; use pcm_f32le (default) or pcm_s16le, optionally with a rate such as pcm_s16le_24000"
        ));
    };
    let sample_rate = match rest.strip_prefix('_') {
        None if rest.is_empty() => 48_000,
        Some(rate) => rate.parse::<u32>().ok().filter(|r| SAMPLE_RATES.contains(r)).ok_or_else(|| {
            format!("unsupported Cartesia sample rate in `{raw}`; use one of 8000, 16000, 22050, 24000, 44100, 48000")
        })?,
        None => return Err(format!("unsupported Cartesia output_format `{raw}`")),
    };
    Ok(CartesiaFormat {
        encoding,
        sample_rate,
    })
}

/// The SSE request body.
pub(crate) fn build_body(text: &str, resolved: &Resolved, format: CartesiaFormat) -> Value {
    let s = &resolved.settings;
    let mut body = Map::new();
    let model = resolved
        .model
        .as_deref()
        .or(defaults::default_model(ProviderType::Cartesia))
        .unwrap_or_default();
    body.insert("model_id".into(), Value::String(model.to_owned()));
    body.insert("transcript".into(), Value::String(text.to_owned()));
    body.insert(
        "voice".into(),
        serde_json::json!({ "id": resolved.voice_id.as_deref().unwrap_or_default() }),
    );
    body.insert(
        "output_format".into(),
        serde_json::json!({ "container": "raw", "encoding": format.encoding_name(), "sample_rate": format.sample_rate }),
    );
    body.insert("add_timestamps".into(), Value::Bool(true));
    // `language` and `locale` are mutually exclusive (400 when both are set).
    if let Some(locale) = get_str(s, "locale") {
        body.insert("locale".into(), Value::String(locale));
    } else if let Some(language) = resolved
        .language
        .as_deref()
        .and_then(settings::primary_language)
    {
        body.insert("language".into(), Value::String(language));
    }
    let mut generation = get_object(s, "generation_config")
        .cloned()
        .unwrap_or_default();
    for key in GENERATION_KEYS {
        if let Some(v) = s.get(key) {
            let v = if key == "emotion" {
                v.clone()
            } else {
                settings::value_f64(v)
                    .and_then(settings::number)
                    .unwrap_or_else(|| v.clone())
            };
            generation.insert(key.to_owned(), v);
        }
    }
    if !generation.is_empty() {
        body.insert("generation_config".into(), Value::Object(generation));
    }
    for key in PASSTHROUGH_KEYS {
        if let Some(v) = s.get(key) {
            body.insert(key.to_owned(), v.clone());
        }
    }
    Value::Object(body)
}

/// Accumulates SSE frames.
#[derive(Debug, Default)]
pub(crate) struct StreamState {
    pub audio: Vec<u8>,
    pub words: Vec<WordTiming>,
    pub done: bool,
}

impl StreamState {
    /// Handle one `data:` payload.
    pub(crate) fn handle(&mut self, origin: &ProviderInstance, data: &str) -> Result<(), TtsError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let frame: Value = serde_json::from_str(data).map_err(|e| {
            TtsError::new(
                TtsErrorKind::Provider,
                origin,
                format!("malformed Cartesia stream frame: {e}"),
            )
            .with_retryable(true)
        })?;
        match frame.get("type").and_then(Value::as_str) {
            Some("chunk") | None if frame.get("data").is_some_and(Value::is_string) => {
                let b64 = frame
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.audio.extend(decode_base64(origin, b64)?);
                if self.audio.len() > http::MAX_RESPONSE_BYTES {
                    return Err(http::too_large(
                        origin,
                        "the streamed audio",
                        http::MAX_RESPONSE_BYTES,
                    ));
                }
                if frame.get("done").and_then(Value::as_bool) == Some(true) {
                    self.done = true;
                }
            }
            Some("timestamps") => {
                if let Some(wt) = frame.get("word_timestamps") {
                    self.push_timestamps(wt);
                }
            }
            Some("done") => self.done = true,
            Some("error") => return Err(stream_error(origin, &frame)),
            _ => {
                if frame.get("done").and_then(Value::as_bool) == Some(true) {
                    self.done = true;
                }
            }
        }
        Ok(())
    }

    fn push_timestamps(&mut self, wt: &Value) {
        let array = |k: &str| {
            wt.get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let (words, starts, ends) = (array("words"), array("start"), array("end"));
        for ((w, s), e) in words.iter().zip(&starts).zip(&ends) {
            if let (Some(text), Some(start), Some(end)) = (w.as_str(), s.as_f64(), e.as_f64()) {
                self.words.push(WordTiming {
                    text: text.to_owned(),
                    start_secs: start,
                    end_secs: end,
                });
            }
        }
    }
}

/// Classify an in-stream `error` event like an HTTP error with its `status_code`.
fn stream_error(origin: &ProviderInstance, frame: &Value) -> TtsError {
    let status = frame
        .get("status_code")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .unwrap_or(500);
    // Drop the framing fields so `"type": "error"` is not mistaken for an error code.
    let mut fields = frame.as_object().cloned().unwrap_or_default();
    for key in ["type", "done", "status_code", "context_id"] {
        fields.remove(key);
    }
    let mut info = http::parse_error_body(&Value::Object(fields).to_string());
    if info.message.is_none() {
        info.message = Some("Cartesia reported an error in the audio stream".to_owned());
    }
    let mut error = http::status_error(origin, status, &info, None);
    error.message = format!("stream error: {}", error.message);
    error
}

/// The base URL (no trailing slash).
pub(crate) fn base_url(origin: &ProviderInstance) -> String {
    http::trim_base(
        settings::non_empty(origin.base_url.as_deref())
            .or(defaults::default_base_url(ProviderType::Cartesia))
            .unwrap_or_default(),
    )
}

/// The Bearer header for requests to `url`, and the key to scrub from messages.
fn headers(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    url: &str,
) -> Result<(HeaderValue, Vec<String>), TtsError> {
    let key = keys::require(ctx.env, origin, url)?;
    let header = http::secret_header(origin, &format!("Bearer {key}"))?;
    Ok((header, keys::secrets_of(&key)))
}

pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    if resolved.voice_id.is_none() {
        return Err(missing_voice(origin));
    }
    let url = format!("{}/tts/sse", base_url(origin));
    let (auth, secrets) = headers(ctx, origin, &url)?;
    let format = parse_output_format(origin.output_format.as_deref()).map_err(|m| {
        TtsError::new(TtsErrorKind::InvalidRequest, origin, m)
            .with_hint("fix `output_format` on the provider")
    })?;
    let body = build_body(&req.text, resolved, format);
    let client = ctx.http.get(origin, &url, request_timeout(origin))?;
    let url_ref = url.as_str();
    let state = with_retries(
        origin,
        ctx.retry,
        Deadline::for_instance(origin),
        &secrets,
        |_, left| {
            // The time left bounds the whole stream; the client's idle read timeout bounds silence.
            let request = client
                .post(url_ref)
                .header(AUTHORIZATION, auth.clone())
                .header(VERSION_HEADER, CARTESIA_API_VERSION)
                .header(ACCEPT, "text/event-stream")
                .json(&body)
                .timeout(left);
            async move {
                let mut response = request
                    .send()
                    .await
                    .map_err(|e| http::transport_error(origin, url_ref, &e))?;
                if !response.status().is_success() {
                    return Err(http::response_error(origin, response).await);
                }
                let mut parser = SseParser::default();
                let mut state = StreamState::default();
                let mut received = 0usize;
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|e| http::transport_error(origin, url_ref, &e))?
                {
                    received += chunk.len();
                    if received > http::MAX_STREAM_BYTES {
                        return Err(http::too_large(
                            origin,
                            "the audio stream",
                            http::MAX_STREAM_BYTES,
                        ));
                    }
                    let events = parser.feed(&chunk).map_err(|e| line_too_long(origin, e))?;
                    for event in events {
                        state.handle(origin, &event.data)?;
                    }
                    if state.done {
                        break;
                    }
                }
                if !state.done {
                    for event in parser.finish() {
                        state.handle(origin, &event.data)?;
                    }
                }
                if !state.done {
                    return Err(TtsError::new(
                        TtsErrorKind::Network,
                        origin,
                        "the Cartesia stream ended before its `done` event",
                    )
                    .with_retryable(true));
                }
                Ok(state)
            }
        },
    )
    .await?;
    let audio = pcm_audio(origin, &state.audio, format.encoding, format.sample_rate)?;
    let words = (!state.words.is_empty()).then_some(state.words);
    Ok(RawSynthesis {
        audio,
        words,
        notes: Vec::new(),
        output_format: Some(format.name()),
    })
}

/// The error of an SSE line or event over the parser's limit.
fn line_too_long(origin: &ProviderInstance, error: crate::sse::LineTooLong) -> TtsError {
    http::too_large(origin, "a line of the audio stream", error.limit)
}

/// Parse one `GET /voices` page: `{data: [...], has_more, next_page}`.
pub(crate) fn parse_voices_page(body: &Value) -> (Vec<Voice>, Option<String>) {
    let voices: Vec<Voice> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|d| d.iter().filter_map(parse_voice).collect())
        .unwrap_or_default();
    let has_more = body
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next = body
        .get("next_page")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| voices.last().map(|v| v.id.clone()))
        .filter(|n| has_more && !n.is_empty());
    (voices, next)
}

fn parse_voice(v: &Value) -> Option<Voice> {
    let id = v.get("id")?.as_str()?.to_owned();
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.trim().is_empty())
    };
    let first_locale = |k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|l| {
                l.as_str()
                    .map(str::to_owned)
                    .or_else(|| l.get("locale").and_then(Value::as_str).map(str::to_owned))
            })
    };
    Some(Voice {
        name: text("name").unwrap_or_else(|| id.clone()),
        language: text("language")
            .or_else(|| first_locale("locales"))
            .or_else(|| first_locale("accents")),
        gender: text("gender"),
        description: text("description").or_else(|| text("tagline")),
        id,
    })
}

async fn get_json(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    url: &str,
    query: &[(&str, String)],
) -> Result<Value, TtsError> {
    let (auth, secrets) = headers(ctx, origin, url)?;
    let client = ctx.http.get(origin, url, request_timeout(origin))?;
    with_retries(
        origin,
        ctx.retry,
        Deadline::for_instance(origin),
        &secrets,
        |_, left| {
            let request = client
                .get(url)
                .query(query)
                .header(AUTHORIZATION, auth.clone())
                .header(VERSION_HEADER, CARTESIA_API_VERSION)
                .timeout(left);
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

pub(crate) async fn list_voices(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
) -> Result<Vec<Voice>, TtsError> {
    let url = format!("{}/voices", base_url(origin));
    let mut voices = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_VOICE_PAGES {
        let mut query = vec![("limit", "100".to_owned())];
        if let Some(c) = &cursor {
            query.push(("starting_after", c.clone()));
        }
        let (page, next) = parse_voices_page(&get_json(ctx, origin, &url, &query).await?);
        voices.extend(page);
        match next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(voices)
}

/// `GET /voices?limit=1`.
pub(crate) async fn check_key(ctx: &Ctx<'_>, origin: &ProviderInstance) -> Result<(), TtsError> {
    let url = format!("{}/voices", base_url(origin));
    get_json(ctx, origin, &url, &[("limit", "1".to_owned())])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, resolved};
    use serde_json::json;

    #[test]
    fn body_shape() {
        let r = resolved(
            ProviderType::Cartesia,
            Some("6ccbfb76"),
            None,
            Some("en-GB"),
            json!({"speed": "1.2", "emotion": "calm", "generation_config": {"volume": 1.5}, "accent": "british", "x": 1}),
        );
        let body = build_body("Hello", &r, parse_output_format(None).unwrap());
        assert_eq!(
            body,
            json!({
                "model_id": "sonic-3.6", "transcript": "Hello", "voice": {"id": "6ccbfb76"},
                "output_format": {"container": "raw", "encoding": "pcm_f32le", "sample_rate": 48000},
                "add_timestamps": true, "language": "en",
                "generation_config": {"volume": 1.5, "speed": 1.2, "emotion": "calm"},
                "accent": "british"
            })
        );
        let with_locale = resolved(
            ProviderType::Cartesia,
            Some("v"),
            None,
            Some("en"),
            json!({"locale": "en-GB"}),
        );
        let body = build_body("x", &with_locale, parse_output_format(None).unwrap());
        assert_eq!(body["locale"], "en-GB");
        assert!(body.get("language").is_none());
    }

    #[test]
    fn output_formats() {
        assert_eq!(parse_output_format(None).unwrap().name(), "pcm_f32le_48000");
        assert_eq!(
            parse_output_format(Some("pcm_s16le")).unwrap().name(),
            "pcm_s16le_48000"
        );
        assert_eq!(
            parse_output_format(Some("PCM_S16LE_24000")).unwrap().name(),
            "pcm_s16le_24000"
        );
        assert!(parse_output_format(Some("pcm_s16le_12345")).is_err());
        assert!(parse_output_format(Some("mp3")).is_err());
        assert!(parse_output_format(Some("pcm_f32lex")).is_err());
    }

    #[test]
    fn stream_frames() {
        let o = instance(ProviderType::Cartesia);
        let mut s = StreamState::default();
        let b64 = |b: &[u8]| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(b)
        };
        s.handle(
            &o,
            &json!({"type": "chunk", "done": false, "status_code": 206, "data": b64(&[1, 2])})
                .to_string(),
        )
        .unwrap();
        s.handle(&o, &json!({"type": "timestamps", "word_timestamps": {"words": ["Hello"], "start": [0.0], "end": [0.4]}}).to_string()).unwrap();
        s.handle(
            &o,
            &json!({"type": "phoneme_timestamps", "phoneme_timestamps": {}}).to_string(),
        )
        .unwrap();
        s.handle(
            &o,
            &json!({"type": "chunk", "data": b64(&[3, 4])}).to_string(),
        )
        .unwrap();
        s.handle(&o, &json!({"type": "timestamps", "word_timestamps": {"words": ["world"], "start": [0.5], "end": [0.9]}}).to_string()).unwrap();
        assert!(!s.done);
        s.handle(&o, &json!({"type": "done", "done": true}).to_string())
            .unwrap();
        assert!(s.done);
        assert_eq!(s.audio, [1, 2, 3, 4]);
        assert_eq!(
            s.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["Hello", "world"]
        );
        let err = StreamState::default()
            .handle(&o, &json!({"type": "error", "status_code": 400, "title": "Invalid model", "message": "no such model", "error_code": "model_not_found"}).to_string())
            .unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::NotFound);
        assert_eq!(err.code.as_deref(), Some("model_not_found"));
        assert!(err.message.contains("no such model"));
        let busy = StreamState::default()
            .handle(
                &o,
                &json!({"type": "error", "status_code": 503, "message": "busy"}).to_string(),
            )
            .unwrap_err();
        assert!(busy.retryable);
        assert!(
            StreamState::default()
                .handle(&o, "not json")
                .unwrap_err()
                .retryable
        );
    }

    #[test]
    fn voices_page() {
        let page = json!({"data": [
            {"id": "a", "name": "Ava", "language": "en", "gender": "feminine", "description": "Warm"},
            {"id": "b", "name": "Ben", "accents": [{"accent": "british", "locale": "en-GB"}], "tagline": "Crisp"}
        ], "has_more": true, "next_page": null});
        let (voices, next) = parse_voices_page(&page);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[1].language.as_deref(), Some("en-GB"));
        assert_eq!(voices[1].description.as_deref(), Some("Crisp"));
        assert_eq!(
            next.as_deref(),
            Some("b"),
            "falls back to the last id as cursor"
        );
        let (_, end) = parse_voices_page(&json!({"data": [{"id": "z"}], "has_more": false}));
        assert_eq!(end, None);
    }
}
