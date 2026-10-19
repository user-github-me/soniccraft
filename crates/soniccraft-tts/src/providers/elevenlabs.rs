//! ElevenLabs: `POST {base}/v1/text-to-speech/{voice_id}/with-timestamps?output_format=…` (`xi-api-key`),
//! returning base64 audio plus a per-character alignment that is grouped into words.
//!
//! Settings: `stability`, `similarity_boost`, `style`, `use_speaker_boost`, `speed` (flat or inside a
//! `voice_settings` object; `eleven_v4*` models only get stability and similarity_boost), `seed` (default: a
//! stable 32-bit hash of voice id and text), `language_code` (default: the voice language, except for models that
//! reject it), and the pass-through body fields `apply_text_normalization`, `apply_language_text_normalization`,
//! `pronunciation_dictionary_locators`, `previous_text`, `next_text`, `use_pvc_as_ivc`.

use reqwest::header::ACCEPT;
use serde::Deserialize;
use serde_json::{Map, Value};
use soniccraft_core::ir::ProviderInstance;
use soniccraft_dsp::AudioBuffer;

use super::{
    Ctx, PcmEncoding, RawSynthesis, decode_base64, decode_encoded, missing_voice, pcm_audio,
};
use crate::defaults::{self, ELEVENLABS_DEFAULT_FORMAT};
use crate::error::{TtsError, TtsErrorKind};
use crate::http::{self, Deadline, encode_path_segment, request_timeout, with_retries};
use crate::keys;
use crate::settings::{self, Resolved, get_bool, get_f64, get_i64, get_object, get_str};
use crate::types::{SynthesisRequest, Voice, WordTiming};

/// Keys of the `voice_settings` object (flat in `settings` or nested in `settings.voice_settings`).
pub(crate) const VOICE_SETTINGS_KEYS: [&str; 5] = [
    "stability",
    "similarity_boost",
    "style",
    "use_speaker_boost",
    "speed",
];
const V4_VOICE_SETTINGS_KEYS: [&str; 2] = ["stability", "similarity_boost"];
const PASSTHROUGH_KEYS: [&str; 6] = [
    "apply_text_normalization",
    "apply_language_text_normalization",
    "pronunciation_dictionary_locators",
    "previous_text",
    "next_text",
    "use_pvc_as_ivc",
];
/// Models that reject `language_code` (only sent to them when `settings.language_code` is explicit).
const NO_LANGUAGE_CODE_MODELS: [&str; 3] = [
    "eleven_multilingual_v2",
    "eleven_multilingual_v1",
    "eleven_monolingual_v1",
];
const MAX_VOICE_PAGES: usize = 50;

/// True for Eleven v4 model ids (`eleven_v4`, `eleven_v4_turbo`, dated snapshots).
pub(crate) fn is_v4(model: &str) -> bool {
    model.starts_with("eleven_v4")
}

/// How to decode a given `output_format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioFormat {
    /// Raw signed 16-bit little-endian mono PCM.
    Pcm { sample_rate: u32 },
    /// A container/codec decoded by the DSP crate (`mp3`, `wav`, `opus`).
    Encoded { extension: &'static str },
}

/// Parse an ElevenLabs `output_format` such as `pcm_24000`, `mp3_44100_128`, `wav_48000` or `opus_48000_64`.
pub(crate) fn parse_output_format(format: &str) -> Result<AudioFormat, String> {
    let lower = format.trim().to_ascii_lowercase();
    if let Some(rate) = lower.strip_prefix("pcm_") {
        return rate
            .parse::<u32>()
            .ok()
            .filter(|r| (8_000..=192_000).contains(r))
            .map(|sample_rate| AudioFormat::Pcm { sample_rate })
            .ok_or_else(|| format!("invalid ElevenLabs output_format `{format}`: expected pcm_<sample rate>, e.g. pcm_24000"));
    }
    for (prefix, extension) in [("mp3_", "mp3"), ("wav_", "wav"), ("opus_", "opus")] {
        if lower.starts_with(prefix) {
            return Ok(AudioFormat::Encoded { extension });
        }
    }
    if lower.starts_with("ulaw_") || lower.starts_with("alaw_") {
        return Err(format!(
            "ElevenLabs output_format `{format}` (8 kHz telephony) is not supported; use pcm_24000"
        ));
    }
    Err(format!(
        "unknown ElevenLabs output_format `{format}`; use pcm_24000 (default), pcm_44100, pcm_48000 or mp3_44100_128"
    ))
}

/// Default `seed`: a stable 32-bit hash of the voice id and text (best-effort reproducibility).
pub(crate) fn default_seed(voice_id: &str, text: &str) -> u32 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"soniccraft.elevenlabs.seed\0");
    hasher.update(voice_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let b = digest.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Coerce known voice-setting types (numbers, booleans), keeping the original value when it does not parse
/// so the API reports the problem.
fn coerce_voice_setting(key: &str, value: &Value, holder: &Map<String, Value>) -> Value {
    let coerced = if key == "use_speaker_boost" {
        get_bool(holder, key).map(Value::Bool)
    } else {
        get_f64(holder, key).and_then(settings::number)
    };
    coerced.unwrap_or_else(|| value.clone())
}

/// Request body plus informational notes (settings dropped for the model).
pub(crate) fn build_body(text: &str, resolved: &Resolved) -> (Value, Vec<String>) {
    let model = resolved
        .model
        .as_deref()
        .or(defaults::default_model(resolved.provider_type))
        .unwrap_or_default();
    let s = &resolved.settings;
    let mut notes = Vec::new();
    let mut body = Map::new();
    body.insert("text".into(), Value::String(text.to_owned()));
    body.insert("model_id".into(), Value::String(model.to_owned()));

    let mut voice_settings = Map::new();
    if let Some(nested) = get_object(s, "voice_settings") {
        for (k, v) in nested {
            if VOICE_SETTINGS_KEYS.contains(&k.as_str()) {
                voice_settings.insert(k.clone(), coerce_voice_setting(k, v, nested));
            }
        }
    }
    for key in VOICE_SETTINGS_KEYS {
        if let Some(v) = s.get(key) {
            voice_settings.insert(key.to_owned(), coerce_voice_setting(key, v, s));
        }
    }
    if is_v4(model) {
        let dropped: Vec<String> = voice_settings
            .keys()
            .filter(|k| !V4_VOICE_SETTINGS_KEYS.contains(&k.as_str()))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            notes.push(format!(
                "`{model}` accepts only stability and similarity_boost; ignoring {}",
                dropped
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            voice_settings.retain(|k, _| V4_VOICE_SETTINGS_KEYS.contains(&k.as_str()));
        }
    }
    if !voice_settings.is_empty() {
        body.insert("voice_settings".into(), Value::Object(voice_settings));
    }

    let voice_id = resolved.voice_id.as_deref().unwrap_or_default();
    let seed = match s.get("seed") {
        None => default_seed(voice_id, text),
        Some(_) => match get_i64(s, "seed").and_then(|v| u32::try_from(v).ok()) {
            Some(seed) => seed,
            None => {
                notes.push("ignoring `seed`: it must be an integer in 0..=4294967295".to_owned());
                default_seed(voice_id, text)
            }
        },
    };
    body.insert("seed".into(), Value::from(seed));

    let language = get_str(s, "language_code").or_else(|| {
        if NO_LANGUAGE_CODE_MODELS.contains(&model) {
            None
        } else {
            resolved
                .language
                .as_deref()
                .and_then(settings::primary_language)
        }
    });
    if let Some(language) = language {
        body.insert("language_code".into(), Value::String(language));
    }
    for key in PASSTHROUGH_KEYS {
        if let Some(v) = s.get(key) {
            body.insert(key.to_owned(), v.clone());
        }
    }
    (Value::Object(body), notes)
}

#[derive(Debug, Deserialize)]
struct TimestampsResponse {
    audio_base64: String,
    #[serde(default)]
    alignment: Option<Alignment>,
    #[serde(default)]
    normalized_alignment: Option<Alignment>,
}

/// Per-character alignment.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Alignment {
    #[serde(default)]
    pub characters: Vec<String>,
    #[serde(default)]
    pub character_start_times_seconds: Vec<f64>,
    #[serde(default)]
    pub character_end_times_seconds: Vec<f64>,
}

impl Alignment {
    fn is_empty(&self) -> bool {
        self.characters.is_empty()
    }
}

/// Group a character alignment into words: maximal runs of non-whitespace characters, with `[audio tags]`
/// dropped (tag characters neither form nor split words).
pub(crate) fn alignment_to_words(alignment: &Alignment) -> Vec<WordTiming> {
    let n = alignment
        .characters
        .len()
        .min(alignment.character_start_times_seconds.len())
        .min(alignment.character_end_times_seconds.len());
    let mut words = Vec::new();
    let mut text = String::new();
    let mut start = 0.0;
    let mut end = 0.0;
    let mut in_tag = false;
    fn flush(text: &mut String, start: f64, end: f64, words: &mut Vec<WordTiming>) {
        if !text.is_empty() {
            words.push(WordTiming {
                text: std::mem::take(text),
                start_secs: start,
                end_secs: end.max(start),
            });
        }
    }
    for i in 0..n {
        let (char_start, char_end) = (
            alignment.character_start_times_seconds[i],
            alignment.character_end_times_seconds[i],
        );
        for c in alignment.characters[i].chars() {
            match c {
                '[' => in_tag = true,
                ']' if in_tag => in_tag = false,
                _ if in_tag => {}
                c if c.is_whitespace() => flush(&mut text, start, end, &mut words),
                c => {
                    if text.is_empty() {
                        start = char_start;
                    }
                    text.push(c);
                    end = char_end;
                }
            }
        }
    }
    flush(&mut text, start, end, &mut words);
    words
}

/// Parse a `/with-timestamps` response into audio bytes and word timings (from `alignment`, else
/// `normalized_alignment`).
pub(crate) fn parse_response(
    origin: &ProviderInstance,
    body: &[u8],
) -> Result<(Vec<u8>, Option<Vec<WordTiming>>), TtsError> {
    let parsed: TimestampsResponse = serde_json::from_slice(body).map_err(|e| {
        TtsError::new(
            TtsErrorKind::Provider,
            origin,
            format!("unexpected ElevenLabs response (expected JSON with audio_base64): {e}"),
        )
    })?;
    let audio = decode_base64(origin, &parsed.audio_base64)?;
    let alignment = parsed
        .alignment
        .filter(|a| !a.is_empty())
        .or(parsed.normalized_alignment.filter(|a| !a.is_empty()));
    let words = alignment
        .map(|a| alignment_to_words(&a))
        .filter(|w| !w.is_empty());
    Ok((audio, words))
}

/// The base URL (no trailing slash).
pub(crate) fn base_url(origin: &ProviderInstance) -> String {
    http::trim_base(
        settings::non_empty(origin.base_url.as_deref())
            .or(defaults::default_base_url(origin.provider_type))
            .unwrap_or_default(),
    )
}

/// Codes that trigger the output-format fallback to `pcm_24000`.
const FORMAT_REJECTION_CODES: [&str; 3] = [
    "subscription_required",
    "feature_not_available",
    "invalid_output_format",
];

/// Classify an ElevenLabs error response. The code driving the output-format fallback is surfaced as the
/// error's `code` even when the envelope lists another code first.
fn status_error(
    origin: &ProviderInstance,
    status: u16,
    body: &str,
    retry_after: Option<f64>,
) -> TtsError {
    let info = http::parse_error_body(body);
    let mut error = http::status_error(origin, status, &info, retry_after);
    if let Some(code) = FORMAT_REJECTION_CODES.iter().find(|c| info.has_code(c)) {
        error.code = Some((*code).to_owned());
    }
    error
}

/// True when the error means "this output format is not available on your plan".
fn is_format_rejection(error: &TtsError) -> bool {
    matches!(
        (error.status, error.code.as_deref()),
        (
            Some(403),
            Some("subscription_required" | "feature_not_available")
        ) | (Some(400), Some("invalid_output_format"))
    )
}

pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    let voice_id = resolved
        .voice_id
        .as_deref()
        .ok_or_else(|| missing_voice(origin))?;
    let url = format!(
        "{}/v1/text-to-speech/{}/with-timestamps",
        base_url(origin),
        encode_path_segment(voice_id)
    );
    let key = keys::require(ctx.env, origin, &url)?;
    let secrets = keys::secrets_of(&key);
    let key = http::secret_header(origin, &key)?;
    let requested = settings::non_empty(origin.output_format.as_deref())
        .unwrap_or(ELEVENLABS_DEFAULT_FORMAT)
        .to_ascii_lowercase();
    let format = parse_output_format(&requested).map_err(|m| {
        TtsError::new(TtsErrorKind::InvalidRequest, origin, m)
            .with_hint("fix `output_format` on the provider")
    })?;
    let (body, mut notes) = build_body(&req.text, resolved);
    // One deadline for the cue, fallback request included.
    let call = Call {
        ctx,
        origin,
        url: &url,
        key: &key,
        secrets: &secrets,
        deadline: Deadline::for_instance(origin),
        body: &body,
    };

    let (audio, words, used) = match call.run(&requested, format).await {
        Ok((audio, words)) => (audio, words, requested),
        Err(error) if requested != ELEVENLABS_DEFAULT_FORMAT && is_format_rejection(&error) => {
            notes.push(format!(
                "output_format `{requested}` is not available on this ElevenLabs plan ({}); fell back to \
                 {ELEVENLABS_DEFAULT_FORMAT}",
                error.code.as_deref().unwrap_or("rejected")
            ));
            let fallback = AudioFormat::Pcm {
                sample_rate: 24_000,
            };
            let (audio, words) = call.run(ELEVENLABS_DEFAULT_FORMAT, fallback).await?;
            (audio, words, ELEVENLABS_DEFAULT_FORMAT.to_owned())
        }
        Err(error) => return Err(error),
    };
    Ok(RawSynthesis {
        audio,
        words,
        notes,
        output_format: Some(used),
    })
}

/// One synthesis call's fixed parts.
struct Call<'a, 'c> {
    ctx: &'a Ctx<'c>,
    origin: &'a ProviderInstance,
    url: &'a str,
    key: &'a reqwest::header::HeaderValue,
    secrets: &'a [String],
    deadline: Deadline,
    body: &'a Value,
}

impl Call<'_, '_> {
    /// One synthesis request in `output_format` (with retries), decoded.
    async fn run(
        &self,
        output_format: &str,
        format: AudioFormat,
    ) -> Result<(AudioBuffer, Option<Vec<WordTiming>>), TtsError> {
        let (origin, url) = (self.origin, self.url);
        let client = self.ctx.http.get(origin, url, request_timeout(origin))?;
        let bytes = with_retries(
            origin,
            self.ctx.retry,
            self.deadline,
            self.secrets,
            |_attempt, left| {
                let request = client
                    .post(url)
                    .query(&[("output_format", output_format)])
                    .header("xi-api-key", self.key.clone())
                    .header(ACCEPT, "application/json")
                    .json(self.body)
                    .timeout(left);
                async move {
                    let response = request
                        .send()
                        .await
                        .map_err(|e| http::transport_error(origin, url, &e))?;
                    if !response.status().is_success() {
                        let e = http::read_error(response).await;
                        if let Some(redirect) =
                            http::redirect_error(origin, e.status, e.location.as_deref())
                        {
                            return Err(redirect);
                        }
                        return Err(status_error(origin, e.status, &e.body, e.retry_after));
                    }
                    http::read_body(origin, url, response, http::MAX_RESPONSE_BYTES).await
                }
            },
        )
        .await?;
        let (audio_bytes, words) = parse_response(origin, &bytes)?;
        let audio = match format {
            AudioFormat::Pcm { sample_rate } => {
                pcm_audio(origin, &audio_bytes, PcmEncoding::S16le, sample_rate)?
            }
            AudioFormat::Encoded { extension } => {
                decode_encoded(origin, audio_bytes, Some(extension)).await?
            }
        };
        Ok((audio, words))
    }
}

/// Parse one `GET /v2/voices` page.
pub(crate) fn parse_voices_page(body: &Value) -> (Vec<Voice>, Option<String>) {
    let voices = body
        .get("voices")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_voice).collect())
        .unwrap_or_default();
    let has_more = body
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let next = body
        .get("next_page_token")
        .and_then(Value::as_str)
        .filter(|t| has_more && !t.is_empty())
        .map(str::to_owned);
    (voices, next)
}

fn parse_voice(v: &Value) -> Option<Voice> {
    let id = v.get("voice_id")?.as_str()?.to_owned();
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let labels = v.get("labels").and_then(Value::as_object);
    let label = |k: &str| {
        labels
            .and_then(|l| l.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
    };
    let language = label("language").or_else(|| {
        v.get("verified_languages")
            .and_then(Value::as_array)
            .and_then(|l| l.first())
            .and_then(|l| l.get("language"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| label("description"))
        .or_else(|| {
            let parts: Vec<String> = ["accent", "age", "use_case"]
                .iter()
                .filter_map(|k| label(k))
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        });
    Some(Voice {
        id,
        name,
        language,
        gender: label("gender"),
        description,
    })
}

pub(crate) async fn list_voices(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
) -> Result<Vec<Voice>, TtsError> {
    let url = format!("{}/v2/voices", base_url(origin));
    let key = keys::require(ctx.env, origin, &url)?;
    let secrets = keys::secrets_of(&key);
    let key = http::secret_header(origin, &key)?;
    let timeout = request_timeout(origin);
    let client = ctx.http.get(origin, &url, timeout)?;
    let mut voices = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_VOICE_PAGES {
        let mut query = vec![
            ("page_size", "100".to_owned()),
            ("include_total_count", "false".to_owned()),
        ];
        if let Some(t) = &token {
            query.push(("next_page_token", t.clone()));
        }
        let body = get_json(ctx, origin, &client, &url, &key, &secrets, &query).await?;
        let (page, next) = parse_voices_page(&body);
        voices.extend(page);
        match next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    Ok(voices)
}

async fn get_json(
    ctx: &Ctx<'_>,
    origin: &ProviderInstance,
    client: &reqwest::Client,
    url: &str,
    key: &reqwest::header::HeaderValue,
    secrets: &[String],
    query: &[(&str, String)],
) -> Result<Value, TtsError> {
    with_retries(
        origin,
        ctx.retry,
        Deadline::for_instance(origin),
        secrets,
        |_, left| {
            let request = client
                .get(url)
                .query(query)
                .header("xi-api-key", key.clone())
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

/// `GET /v1/user`. A key that is valid but lacks the `user_read` scope still counts as working.
pub(crate) async fn check_key(ctx: &Ctx<'_>, origin: &ProviderInstance) -> Result<(), TtsError> {
    let url = format!("{}/v1/user", base_url(origin));
    let key = keys::require(ctx.env, origin, &url)?;
    let secrets = keys::secrets_of(&key);
    let key = http::secret_header(origin, &key)?;
    let client = ctx.http.get(origin, &url, request_timeout(origin))?;
    match get_json(ctx, origin, &client, &url, &key, &secrets, &[]).await {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(e.status, Some(401 | 403))
                && (e.code.as_deref() == Some("missing_permissions")
                    || e.message.contains("permission")) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, resolved};
    use serde_json::json;
    use soniccraft_core::ir::ProviderType;

    #[test]
    fn body_maps_settings_and_defaults() {
        let r = resolved(
            ProviderType::Elevenlabs,
            Some("JBFqnCBsd6RMkjVDRZzb"),
            None,
            Some("en-US"),
            json!({"stability": 0.4, "similarity_boost": "0.8", "speed": 1.1, "use_speaker_boost": "true",
                   "apply_text_normalization": "off", "unrelated": 1}),
        );
        let (body, notes) = build_body("Welcome to SonicCraft.", &r);
        assert!(notes.is_empty());
        assert_eq!(body["text"], "Welcome to SonicCraft.");
        assert_eq!(body["model_id"], "eleven_multilingual_v2");
        assert_eq!(
            body["voice_settings"],
            json!({"stability": 0.4, "similarity_boost": 0.8, "speed": 1.1, "use_speaker_boost": true})
        );
        assert_eq!(
            body["seed"],
            json!(default_seed(
                "JBFqnCBsd6RMkjVDRZzb",
                "Welcome to SonicCraft."
            ))
        );
        // multilingual_v2 rejects language_code unless set explicitly.
        assert!(body.get("language_code").is_none());
        assert_eq!(body["apply_text_normalization"], "off");
        assert!(body.get("unrelated").is_none());
    }

    #[test]
    fn v4_drops_style_and_speed_and_sends_language() {
        let r = resolved(
            ProviderType::Elevenlabs,
            Some("v"),
            Some("eleven_v4"),
            Some("de-DE"),
            json!({"voice_settings": {"stability": 0.5, "style": 0.3}, "speed": 1.2, "similarity_boost": 0.7, "seed": 7}),
        );
        let (body, notes) = build_body("Hallo", &r);
        assert_eq!(
            body["voice_settings"],
            json!({"stability": 0.5, "similarity_boost": 0.7})
        );
        assert_eq!(body["seed"], 7);
        assert_eq!(body["language_code"], "de");
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("`style`") && notes[0].contains("`speed`"),
            "{notes:?}"
        );
    }

    #[test]
    fn invalid_seed_falls_back_with_note() {
        let r = resolved(
            ProviderType::Elevenlabs,
            Some("v"),
            None,
            None,
            json!({"seed": -5}),
        );
        let (body, notes) = build_body("x", &r);
        assert_eq!(body["seed"], json!(default_seed("v", "x")));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn seed_is_stable() {
        assert_eq!(default_seed("voice", "text"), default_seed("voice", "text"));
        assert_ne!(
            default_seed("voice", "text"),
            default_seed("voice", "text!")
        );
        assert_ne!(default_seed("a", "bc"), default_seed("ab", "c"));
    }

    #[test]
    fn output_formats() {
        assert_eq!(
            parse_output_format("pcm_24000"),
            Ok(AudioFormat::Pcm {
                sample_rate: 24_000
            })
        );
        assert_eq!(
            parse_output_format("PCM_44100"),
            Ok(AudioFormat::Pcm {
                sample_rate: 44_100
            })
        );
        assert_eq!(
            parse_output_format("mp3_44100_128"),
            Ok(AudioFormat::Encoded { extension: "mp3" })
        );
        assert_eq!(
            parse_output_format("wav_48000"),
            Ok(AudioFormat::Encoded { extension: "wav" })
        );
        assert!(parse_output_format("pcm_abc").is_err());
        assert!(
            parse_output_format("ulaw_8000")
                .unwrap_err()
                .contains("not supported")
        );
        assert!(parse_output_format("flac").is_err());
    }

    #[test]
    fn alignment_groups_words_and_drops_tags() {
        let text = "[whispering] Hi, there [laughs]friend";
        let chars: Vec<String> = text.chars().map(String::from).collect();
        let starts: Vec<f64> = (0..chars.len()).map(|i| i as f64 * 0.1).collect();
        let ends: Vec<f64> = (0..chars.len()).map(|i| i as f64 * 0.1 + 0.1).collect();
        let a = Alignment {
            characters: chars,
            character_start_times_seconds: starts,
            character_end_times_seconds: ends,
        };
        let words = alignment_to_words(&a);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Hi,", "there", "friend"]);
        assert!((words[0].start_secs - 1.3).abs() < 1e-9);
        assert!((words[0].end_secs - 1.6).abs() < 1e-9);
        assert!((words[2].start_secs - 3.1).abs() < 1e-9);
        assert!((words[2].end_secs - 3.7).abs() < 1e-9);
    }

    #[test]
    fn response_prefers_alignment_then_normalized() {
        let o = instance(ProviderType::Elevenlabs);
        let audio = base64_of(&[1, 0, 2, 0]);
        let body = json!({
            "audio_base64": audio,
            "alignment": null,
            "normalized_alignment": {"characters": ["h", "i"], "character_start_times_seconds": [0.0, 0.1],
                                     "character_end_times_seconds": [0.1, 0.2]}
        });
        let (bytes, words) = parse_response(&o, body.to_string().as_bytes()).unwrap();
        assert_eq!(bytes, [1, 0, 2, 0]);
        assert_eq!(words.unwrap()[0].text, "hi");
        let no_words = json!({"audio_base64": base64_of(&[0, 0])});
        assert_eq!(
            parse_response(&o, no_words.to_string().as_bytes())
                .unwrap()
                .1,
            None
        );
        let err = parse_response(&o, b"{\"nope\":1}").unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Provider);
    }

    #[test]
    fn voices_page_parsing() {
        let page = json!({
            "voices": [
                {"voice_id": "a1", "name": "Aria", "labels": {"gender": "female", "accent": "american", "age": "young"},
                 "verified_languages": [{"language": "en"}]},
                {"voice_id": "b2", "name": "Bob", "description": "Deep narrator", "labels": {"language": "de"}},
                {"name": "no id"}
            ],
            "has_more": true,
            "next_page_token": "tok"
        });
        let (voices, next) = parse_voices_page(&page);
        assert_eq!(next.as_deref(), Some("tok"));
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].language.as_deref(), Some("en"));
        assert_eq!(voices[0].gender.as_deref(), Some("female"));
        assert_eq!(voices[0].description.as_deref(), Some("american, young"));
        assert_eq!(voices[1].language.as_deref(), Some("de"));
        assert_eq!(voices[1].description.as_deref(), Some("Deep narrator"));
        let (_, none) =
            parse_voices_page(&json!({"voices": [], "has_more": false, "next_page_token": "x"}));
        assert_eq!(none, None);
    }

    fn base64_of(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}
