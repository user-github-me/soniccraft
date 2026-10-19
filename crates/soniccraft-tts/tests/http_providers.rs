//! The real HTTP paths of the network providers, against an in-process HTTP/1.1 server: request shapes,
//! response parsing, retries, error mapping, SSE, fallbacks, voice listing, credential checks and
//! concurrency limits. No real network and no API keys.

mod support;

use std::time::{Duration, Instant};

use serde_json::json;
use soniccraft_core::ir::ProviderType;
use soniccraft_tts::{EnvSource, TtsClient, TtsErrorKind, TtsOptions, error_codes};
use support::{Reply, TestServer, b64, instance, options, pcm_f32, pcm16, request, voice};

fn alignment(text: &str, step: f64) -> serde_json::Value {
    let chars: Vec<String> = text.chars().map(String::from).collect();
    let starts: Vec<f64> = (0..chars.len()).map(|i| i as f64 * step).collect();
    let ends: Vec<f64> = (0..chars.len()).map(|i| (i + 1) as f64 * step).collect();
    json!({"characters": chars, "character_start_times_seconds": starts, "character_end_times_seconds": ends})
}

const VOICE: &str = "JBFqnCBsd6RMkjVDRZzb";
const EL_PATH: &str = "/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb/with-timestamps";

#[tokio::test]
async fn elevenlabs_json_base64_and_alignment() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        EL_PATH,
        vec![Reply::json(
            200,
            json!({"audio_base64": b64(&pcm16(2_400)), "alignment": alignment("Hi there", 0.01),
                                     "normalized_alignment": null}),
        )],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(
            ProviderType::Elevenlabs,
            Some(VOICE),
            json!({"stability": 0.4, "similarity_boost": 0.8, "seed": 42}),
        ),
        "Hi there",
    );
    let result = client.synthesize(&req).await.unwrap();
    assert!(!result.cache_hit);
    assert_eq!(
        (
            result.audio.sample_rate,
            result.audio.frames(),
            result.audio.num_channels()
        ),
        (24_000, 2_400, 1)
    );
    let words = result.words.unwrap();
    assert_eq!(
        words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
        ["Hi", "there"]
    );
    assert!((words[1].start_secs - 0.03).abs() < 1e-9 && (words[1].end_secs - 0.08).abs() < 1e-9);

    let sent = &server.requests()[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.query, "output_format=pcm_24000");
    assert_eq!(sent.header("xi-api-key"), Some("el-test-key"));
    assert_eq!(sent.header("authorization"), None);
    assert_eq!(
        sent.json(),
        json!({"text": "Hi there", "model_id": "eleven_multilingual_v2",
               "voice_settings": {"stability": 0.4, "similarity_boost": 0.8}, "seed": 42})
    );
}

#[tokio::test]
async fn elevenlabs_falls_back_to_pcm_24000_when_the_plan_rejects_a_format() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        EL_PATH,
        vec![
            Reply::json(
                403,
                json!({"detail": {"type": "authorization_error", "code": "subscription_required",
                                                "message": "pcm_44100 requires the Pro tier"}}),
            ),
            Reply::json(
                200,
                json!({"audio_base64": b64(&pcm16(480)), "alignment": null}),
            ),
        ],
    );
    let mut provider = instance(ProviderType::Elevenlabs, Some(server.url()));
    provider.output_format = Some("pcm_44100".into());
    let client = TtsClient::new(options());
    let result = client
        .synthesize(&request(
            provider,
            voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
            "Hello",
        ))
        .await
        .unwrap();
    assert_eq!(result.audio.sample_rate, 24_000);
    assert_eq!(result.words, None);
    assert!(
        result
            .notes
            .iter()
            .any(|n| n.contains("fell back to pcm_24000")),
        "{:?}",
        result.notes
    );
    let queries: Vec<String> = server.requests().into_iter().map(|r| r.query).collect();
    assert_eq!(
        queries,
        ["output_format=pcm_44100", "output_format=pcm_24000"]
    );
}

#[tokio::test]
async fn rate_limits_are_retried_honouring_retry_after() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        EL_PATH,
        vec![
            Reply::json(
                429,
                json!({"detail": {"type": "rate_limit_error", "code": "concurrent_limit_exceeded",
                                                "message": "too many concurrent requests"}}),
            )
            .header("Retry-After", "4"),
            Reply::json(200, json!({"audio_base64": b64(&pcm16(240))})),
        ],
    );
    // retry_base_delay 20 ms scales Retry-After: 4 s → 80 ms.
    let client = TtsClient::new(TtsOptions {
        retry_base_delay: Duration::from_millis(20),
        ..options()
    });
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
        "Hello",
    );
    let started = Instant::now();
    let result = client.synthesize(&req).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(80),
        "waited {:?}",
        started.elapsed()
    );
    assert_eq!(result.audio.frames(), 240);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn auth_errors_fail_fast_with_an_actionable_hint() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        EL_PATH,
        vec![Reply::json(401, json!({"detail": {"type": "authentication_error", "code": "invalid_api_key",
                                                 "message": "Invalid API key", "status": "invalid_api_key"}}))],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(
        (err.kind, err.status, err.attempts, err.retryable),
        (TtsErrorKind::Auth, Some(401), 1, false)
    );
    assert_eq!(err.code.as_deref(), Some("invalid_api_key"));
    assert_eq!(err.message, "Invalid API key");
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("ELEVENLABS_API_KEY")
    );
    assert_eq!(server.requests().len(), 1);
    for rendered in [
        format!("{err}"),
        format!("{err:?}"),
        serde_json::to_string(&err).unwrap(),
    ] {
        assert!(!rendered.contains("el-test-key"), "key leaked: {rendered}");
    }
}

#[tokio::test]
async fn unknown_voice_points_at_the_voices_command() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/text-to-speech/nope/with-timestamps",
        vec![Reply::json(404, json!({"detail": {"type": "not_found", "code": "voice_not_found", "message": "Voice nope was not found"}}))],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(ProviderType::Elevenlabs, Some("nope"), json!({})),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::NotFound);
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("soniccraft voices elevenlabs"),
        "{err:?}"
    );
}

#[tokio::test]
async fn validation_errors_surface_the_provider_message() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        EL_PATH,
        vec![Reply::json(422, json!({"detail": [{"loc": ["body", "voice_settings", "stability"], "msg": "must be <= 1", "type": "less_than_equal"}]}))],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(
            ProviderType::Elevenlabs,
            Some(VOICE),
            json!({"stability": 7}),
        ),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(
        (err.kind, err.status, err.attempts),
        (TtsErrorKind::InvalidRequest, Some(422), 1)
    );
    assert_eq!(err.message, "body.voice_settings.stability: must be <= 1");
}

#[tokio::test]
async fn missing_keys_are_reported_before_any_request() {
    let server = TestServer::start().await;
    let client = TtsClient::new(TtsOptions {
        env: EnvSource::fixed([("UNRELATED", "1")]),
        ..options()
    });
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Auth);
    assert!(err.message.contains("ELEVENLABS_API_KEY"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn server_errors_exhaust_the_retries() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        EL_PATH,
        vec![Reply::json(
            500,
            json!({"detail": {"code": "internal_error", "message": "boom"}}),
        )],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(
        (err.kind, err.retryable, err.attempts),
        (TtsErrorKind::Provider, true, 4)
    );
    assert_eq!(server.requests().len(), 4);
    assert!(err.to_string().contains("after 4 attempts"), "{err}");
}

#[tokio::test]
async fn connection_failures_are_network_errors_and_local_refusals_are_not_retried() {
    // Nothing listens on a local port: a definitive failure, reported after one attempt.
    let port = support::closed_port();
    let client = TtsClient::new(options());
    let req = request(
        instance(
            ProviderType::Openai,
            Some(format!("http://127.0.0.1:{port}/v1")),
        ),
        voice(ProviderType::Openai, None, json!({})),
        "Hello",
    );
    let started = Instant::now();
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(
        (err.kind, err.retryable, err.attempts),
        (TtsErrorKind::Network, false, 1)
    );
    assert_eq!(err.code.as_deref(), Some(error_codes::CONNECT_FAILED));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("start the local server"),
        "{err:?}"
    );

    // A server that hangs up mid-request is a transient network error, retried.
    let server = TestServer::start().await;
    server.route("POST", "/v1/audio/speech", vec![Reply::hangup()]);
    let req = request(
        instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
        voice(ProviderType::Openai, None, json!({})),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Network);
    assert_eq!(server.requests().len(), 4);
}

#[tokio::test]
async fn openai_compatible_raw_pcm() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::bytes(200, "application/octet-stream", pcm16(4_800))],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Openai, Some(format!("{}/v1/", server.url()))),
        voice(
            ProviderType::Openai,
            Some("coral"),
            json!({"speed": 1.25, "instructions": "Calm."}),
        ),
        "Hello there",
    );
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(
        (result.audio.sample_rate, result.audio.frames()),
        (24_000, 4_800)
    );
    assert_eq!(result.words, None);
    let sent = &server.requests()[0];
    assert_eq!(sent.path, "/v1/audio/speech");
    assert_eq!(sent.header("authorization"), Some("Bearer sk-test-key"));
    assert_eq!(
        sent.json(),
        json!({"model": "gpt-4o-mini-tts", "input": "Hello there", "voice": "coral", "response_format": "pcm",
               "instructions": "Calm.", "speed": 1.25})
    );
}

#[tokio::test]
async fn openai_billing_429_is_not_retried_and_text_plain_json_is_parsed() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::bytes(
            429,
            "text/plain",
            json!({"error": {"message": "You exceeded your current quota.", "type": "insufficient_quota", "code": "insufficient_quota"}})
                .to_string(),
        )],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
        voice(ProviderType::Openai, None, json!({})),
        "Hello",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(
        (err.kind, err.retryable, err.attempts),
        (TtsErrorKind::QuotaExceeded, false, 1)
    );
    assert_eq!(err.message, "You exceeded your current quota.");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn openai_compatible_server_without_key_sends_no_authorization() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::bytes(200, "audio/pcm", pcm16(100))],
    );
    let client = TtsClient::new(TtsOptions {
        env: EnvSource::fixed([("X", "1")]),
        ..options()
    });
    let req = request(
        instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
        voice(
            ProviderType::Openai,
            Some("af_heart"),
            json!({"extra_body": {"lang_code": "a"}}),
        ),
        "Hi",
    );
    client.synthesize(&req).await.unwrap();
    let sent = &server.requests()[0];
    assert_eq!(sent.header("authorization"), None);
    assert_eq!(sent.json()["lang_code"], "a");
}

fn cartesia_frames(first: &[u8], second: &[u8]) -> Vec<serde_json::Value> {
    vec![
        json!({"type": "chunk", "done": false, "status_code": 206, "step_time": 12, "context_id": "c", "data": b64(first)}),
        json!({"type": "timestamps", "done": false, "status_code": 206, "context_id": "c",
               "word_timestamps": {"words": ["Hello"], "start": [0.0], "end": [0.4]}}),
        json!({"type": "chunk", "done": false, "status_code": 206, "context_id": "c", "data": b64(second)}),
        json!({"type": "timestamps", "done": false, "status_code": 206, "context_id": "c",
               "word_timestamps": {"words": ["world", "again"], "start": [0.5, 0.9], "end": [0.8, 1.2]}}),
        json!({"type": "done", "done": true, "status_code": 206, "context_id": "c"}),
    ]
}

#[tokio::test]
async fn cartesia_sse_assembles_audio_and_batched_timestamps() {
    let samples: Vec<f32> = (0..48_000)
        .map(|i| ((i % 480) as f32 / 480.0) - 0.5)
        .collect();
    let bytes = pcm_f32(&samples);
    // Split the PCM mid-sample: chunk boundaries need not align with frames.
    let (first, second) = bytes.split_at(6 * 1_000 + 2);
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/tts/sse",
        vec![Reply::sse(&cartesia_frames(first, second))],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Cartesia, Some(server.url())),
        voice(
            ProviderType::Cartesia,
            Some("6ccbfb76-1fc6-48f7-b71d-91ac6298247b"),
            json!({"speed": 1.1}),
        ),
        "Hello world again",
    );
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(result.audio.sample_rate, 48_000);
    assert_eq!(result.audio.channels[0], samples);
    let words = result.words.unwrap();
    assert_eq!(
        words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
        ["Hello", "world", "again"]
    );
    assert_eq!(
        (words[2].start_secs, words[2].end_secs),
        (0.9, 1.0),
        "clamped to the 1 s of audio"
    );

    let sent = &server.requests()[0];
    assert_eq!(sent.header("authorization"), Some("Bearer sk_car_test-key"));
    assert_eq!(sent.header("cartesia-version"), Some("2026-08-14"));
    assert_eq!(sent.header("accept"), Some("text/event-stream"));
    let body = sent.json();
    assert_eq!(
        body["voice"],
        json!({"id": "6ccbfb76-1fc6-48f7-b71d-91ac6298247b"})
    );
    assert_eq!(
        body["output_format"],
        json!({"container": "raw", "encoding": "pcm_f32le", "sample_rate": 48000})
    );
    assert_eq!(body["add_timestamps"], true);
    assert_eq!(body["generation_config"], json!({"speed": 1.1}));
    assert_eq!(body["model_id"], "sonic-3.6");
}

#[tokio::test]
async fn cartesia_error_event_inside_a_200_stream_fails() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/tts/sse",
        vec![Reply::sse(&[
            json!({"type": "chunk", "done": false, "data": b64(&pcm_f32(&[0.1, 0.2]))}),
            json!({"type": "error", "done": true, "status_code": 400, "title": "Invalid voice",
                   "message": "Voice not found for id v", "error_code": "voice_not_found"}),
        ])],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Cartesia, Some(server.url())),
        voice(ProviderType::Cartesia, Some("v"), json!({})),
        "Hi",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::NotFound);
    assert_eq!(err.status, Some(400));
    assert!(err.message.contains("Voice not found"), "{err}");
    assert_eq!(server.requests().len(), 1, "not retried");
}

#[tokio::test]
async fn cartesia_truncated_stream_is_retried() {
    let server = TestServer::start().await;
    let audio = pcm_f32(&[0.25; 480]);
    server.route(
        "POST",
        "/tts/sse",
        vec![
            Reply::sse(&[json!({"type": "chunk", "done": false, "data": b64(&audio)})]),
            Reply::sse(&cartesia_frames(&audio[..1_000], &audio[1_000..])),
        ],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Cartesia, Some(server.url())),
        voice(ProviderType::Cartesia, Some("v"), json!({})),
        "Hi",
    );
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(result.audio.frames(), 480);
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn kokoro_captioned_speech_folds_punctuation() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/dev/captioned_speech",
        vec![Reply::json(200, json!({
            "audio": b64(&pcm16(24_000)),
            "audio_format": "audio/pcm",
            "timestamps": [
                {"word": "Hello", "start_time": 0.275, "end_time": 0.5}, {"word": ",", "start_time": 0.5, "end_time": 0.5},
                {"word": "world", "start_time": 0.6, "end_time": 0.9}, {"word": "!", "start_time": 0.9, "end_time": 0.95}
            ]
        }))],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Kokoro, Some(format!("{}/v1", server.url()))),
        voice(ProviderType::Kokoro, None, json!({"speed": 1.2})),
        "Hello, world!",
    );
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(
        (result.audio.sample_rate, result.audio.frames()),
        (24_000, 24_000)
    );
    let words = result.words.unwrap();
    assert_eq!(
        words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
        ["Hello,", "world!"]
    );
    let sent = &server.requests()[0];
    assert_eq!(sent.path, "/dev/captioned_speech");
    assert_eq!(
        sent.json(),
        json!({"model": "kokoro", "input": "Hello, world!", "voice": "af_heart", "response_format": "pcm",
               "stream": false, "return_timestamps": true, "speed": 1.2})
    );
}

#[tokio::test]
async fn kokoro_without_captions_falls_back_once_and_remembers() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::bytes(200, "audio/pcm", pcm16(1_200))],
    );
    let client = TtsClient::new(options());
    let make = |text: &str| {
        request(
            instance(ProviderType::Kokoro, Some(server.url())),
            voice(ProviderType::Kokoro, Some("am_adam"), json!({})),
            text,
        )
    };
    let first = client.synthesize(&make("One")).await.unwrap();
    assert_eq!((first.audio.frames(), first.words.clone()), (1_200, None));
    client.synthesize(&make("Two")).await.unwrap();
    let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/dev/captioned_speech",
            "/v1/audio/speech",
            "/v1/audio/speech"
        ]
    );
    let body = server.requests_to("/v1/audio/speech")[0].json();
    assert_eq!(body["stream"], false);
    assert!(body.get("return_timestamps").is_none());
}

#[tokio::test]
async fn voice_listing() {
    let server = TestServer::start().await;
    server.route(
        "GET",
        "/v2/voices",
        vec![
            Reply::json(200, json!({"voices": [{"voice_id": "a", "name": "Aria", "labels": {"gender": "female"}}],
                                    "has_more": true, "next_page_token": "page2"})),
            Reply::json(200, json!({"voices": [{"voice_id": "b", "name": "Bill"}], "has_more": false, "next_page_token": null})),
        ],
    );
    server.route(
        "GET",
        "/voices",
        vec![
            Reply::json(200, json!({"data": [{"id": "c1", "name": "Cleo"}], "has_more": true, "next_page": "c1"})),
            Reply::json(200, json!({"data": [{"id": "c2", "name": "Cory"}], "has_more": false, "next_page": null})),
        ],
    );
    server.route(
        "GET",
        "/v1/audio/voices",
        vec![Reply::json(200, json!({"voices": [{"id": "af_heart", "name": "af_heart"}, {"id": "bm_lewis", "name": "bm_lewis"}],
                                     "default_voice": "af_heart"}))],
    );
    let client = TtsClient::new(options());

    let eleven = client
        .list_voices(&instance(ProviderType::Elevenlabs, Some(server.url())))
        .await
        .unwrap();
    assert_eq!(
        eleven.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
    let el_queries: Vec<String> = server
        .requests_to("/v2/voices")
        .into_iter()
        .map(|r| r.query)
        .collect();
    assert_eq!(
        el_queries,
        [
            "page_size=100&include_total_count=false",
            "page_size=100&include_total_count=false&next_page_token=page2"
        ]
    );

    let cartesia = client
        .list_voices(&instance(ProviderType::Cartesia, Some(server.url())))
        .await
        .unwrap();
    assert_eq!(
        cartesia.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        ["c1", "c2"]
    );
    let cq: Vec<String> = server
        .requests_to("/voices")
        .into_iter()
        .map(|r| r.query)
        .collect();
    assert_eq!(cq, ["limit=100", "limit=100&starting_after=c1"]);
    assert!(
        server
            .requests_to("/voices")
            .iter()
            .all(|r| r.header("cartesia-version") == Some("2026-08-14"))
    );

    let kokoro = client
        .list_voices(&instance(ProviderType::Kokoro, Some(server.url())))
        .await
        .unwrap();
    assert_eq!(kokoro.len(), 2);
    assert_eq!(kokoro[1].language.as_deref(), Some("en-GB"));

    // OpenAI-compatible servers are asked for their voices; OpenAI itself has a static list.
    let compatible = client
        .list_voices(&instance(
            ProviderType::Openai,
            Some(format!("{}/v1", server.url())),
        ))
        .await
        .unwrap();
    assert_eq!(compatible[0].id, "af_heart");
    let official = client
        .list_voices(&instance(ProviderType::Openai, None))
        .await
        .unwrap();
    assert_eq!(official.len(), 13);
    assert!(official.iter().any(|v| v.id == "marin"));
}

#[tokio::test]
async fn online_credential_checks() {
    let server = TestServer::start().await;
    server.route(
        "GET",
        "/v1/user",
        vec![Reply::json(
            200,
            json!({"user_id": "u", "subscription": {"tier": "free"}}),
        )],
    );
    server.route(
        "GET",
        "/voices",
        vec![Reply::json(
            401,
            json!({"error_code": null, "message": "Invalid API key.", "title": "Unauthorized"}),
        )],
    );
    server.route(
        "GET",
        "/v1/models",
        vec![Reply::json(200, json!({"object": "list", "data": []}))],
    );
    server.route(
        "GET",
        "/health",
        vec![Reply::json(200, json!({"status": "healthy"}))],
    );
    let client = TtsClient::new(options());
    client
        .check_credentials_online(&instance(ProviderType::Elevenlabs, Some(server.url())))
        .await
        .unwrap();
    let err = client
        .check_credentials_online(&instance(ProviderType::Cartesia, Some(server.url())))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind, err.message.as_str()),
        (TtsErrorKind::Auth, "Invalid API key.")
    );
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("CARTESIA_API_KEY")
    );
    client
        .check_credentials_online(&instance(
            ProviderType::Openai,
            Some(format!("{}/v1", server.url())),
        ))
        .await
        .unwrap();
    client
        .check_credentials_online(&instance(ProviderType::Kokoro, Some(server.url())))
        .await
        .unwrap();
    assert_eq!(
        server.requests_to("/v1/user")[0].header("xi-api-key"),
        Some("el-test-key")
    );
    assert_eq!(server.requests_to("/voices")[0].query, "limit=1");
}

#[tokio::test]
async fn per_instance_and_global_concurrency_limits() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::bytes(200, "audio/pcm", pcm16(240)).delay(Duration::from_millis(60))],
    );
    let base = format!("{}/v1", server.url());
    let texts = ["a", "b", "c", "d", "e", "f"];

    // Per-instance max_concurrency = 2 under a generous global limit.
    let client = TtsClient::new(TtsOptions {
        jobs: 8,
        ..options()
    });
    let mut provider = instance(ProviderType::Openai, Some(base.clone()));
    provider.max_concurrency = 2;
    let requests: Vec<_> = texts
        .iter()
        .map(|t| {
            request(
                provider.clone(),
                voice(ProviderType::Openai, None, json!({})),
                t,
            )
        })
        .collect();
    let results = client.synthesize_all(requests, |_| {}).await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(server.max_in_flight(), 2);

    // Global jobs = 1 across two instances.
    let server2 = TestServer::start().await;
    server2.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::bytes(200, "audio/pcm", pcm16(240)).delay(Duration::from_millis(30))],
    );
    let client = TtsClient::new(TtsOptions {
        jobs: 1,
        ..options()
    });
    let mut a = instance(ProviderType::Openai, Some(format!("{}/v1", server2.url())));
    a.name = "voice_a".into();
    let mut b = a.clone();
    b.name = "voice_b".into();
    let requests: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            request(
                if i % 2 == 0 { a.clone() } else { b.clone() },
                voice(ProviderType::Openai, None, json!({})),
                t,
            )
        })
        .collect();
    let results = client.synthesize_all(requests, |_| {}).await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(server2.max_in_flight(), 1);
    assert_eq!(server2.requests().len(), 6);
}
