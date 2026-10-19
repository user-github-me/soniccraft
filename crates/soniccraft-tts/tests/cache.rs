//! The on-disk cache: round trips, metadata, corrupt-entry recovery, `read_cache`/`write_cache`, dedup and the
//! HTTP path (a cache hit makes no request).

mod support;

use std::path::Path;

use serde_json::json;
use soniccraft_core::ir::ProviderType;
use soniccraft_tts::{SynthesisRequest, TtsClient, TtsOptions, cache_key, render_mock};
use support::{Reply, TestServer, b64, instance, options, pcm16, request, voice};

fn mock(text: &str) -> SynthesisRequest {
    request(
        instance(ProviderType::Mock, None),
        voice(ProviderType::Mock, Some("narrator"), json!({"wpm": 170})),
        text,
    )
}

fn cached_client(dir: &Path) -> TtsClient {
    TtsClient::new(TtsOptions {
        cache_dir: Some(dir.to_path_buf()),
        ..TtsOptions::default()
    })
}

fn entry_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn round_trip_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let client = cached_client(dir.path());
    let req = mock("Cached words, exactly.");
    let key = cache_key(&req).unwrap();
    assert_eq!(client.cached(&req), None);

    let first = client.synthesize(&req).await.unwrap();
    assert!(!first.cache_hit);
    assert!(first.notes.is_empty(), "{:?}", first.notes);
    assert_eq!(
        entry_files(dir.path()),
        [format!("{key}.json"), format!("{key}.wav")]
    );

    let meta = client.cached(&req).unwrap();
    assert_eq!(meta.frames, first.audio.frames() as u64);
    assert_eq!((meta.sample_rate, meta.channels), (24_000, 1));
    assert_eq!(meta.duration_secs, first.audio.duration_secs());
    assert_eq!(meta.words, first.words);

    let second = client.synthesize(&req).await.unwrap();
    assert!(second.cache_hit);
    assert_eq!(second.cache_key, key);
    assert_eq!(
        second.audio, first.audio,
        "32-bit float WAV round-trips exactly"
    );
    assert_eq!(second.words, first.words);

    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join(format!("{key}.json"))).unwrap())
            .unwrap();
    assert_eq!(json["key"], key.as_str());
    assert_eq!(json["provider_type"], "mock");
    assert_eq!(json["text"], "Cached words, exactly.");
    assert_eq!(json["words"].as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn corrupt_entries_are_ignored_and_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let client = cached_client(dir.path());
    let req = mock("Recover me.");
    let key = cache_key(&req).unwrap();
    let good = client.synthesize(&req).await.unwrap();
    let json_path = dir.path().join(format!("{key}.json"));
    let wav_path = dir.path().join(format!("{key}.wav"));

    // Garbage metadata.
    std::fs::write(&json_path, b"{ definitely not json").unwrap();
    assert_eq!(client.cached(&req), None);
    let again = client.synthesize(&req).await.unwrap();
    assert!(!again.cache_hit);
    assert_eq!(again.audio, good.audio);
    assert!(client.cached(&req).is_some(), "entry rewritten");

    // Truncated audio.
    let bytes = std::fs::read(&wav_path).unwrap();
    std::fs::write(&wav_path, &bytes[..bytes.len() / 3]).unwrap();
    assert_eq!(client.cached(&req), None);
    let again = client.synthesize(&req).await.unwrap();
    assert!(!again.cache_hit);
    assert!(client.synthesize(&req).await.unwrap().cache_hit);

    // Audio replaced by a valid (large enough) WAV whose length disagrees with the metadata.
    std::fs::write(
        &wav_path,
        support::wav_s16(24_000, &support::tone(good.audio.frames() * 3)),
    )
    .unwrap();
    assert!(
        client.cached(&req).is_some(),
        "the cheap check only looks at sizes"
    );
    let again = client.synthesize(&req).await.unwrap();
    assert!(!again.cache_hit);
    assert_eq!(again.audio, good.audio);

    // A NaN and an infinity written into the stored 32-bit float samples (review-tts-security/
    // nonfinite-resample-panic): the entry is re-synthesized, not played with the samples zeroed.
    let mut bytes = std::fs::read(&wav_path).unwrap();
    let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
    bytes[data..data + 4].copy_from_slice(&f32::NAN.to_le_bytes());
    bytes[data + 40..data + 44].copy_from_slice(&f32::INFINITY.to_le_bytes());
    std::fs::write(&wav_path, &bytes).unwrap();
    let again = client.synthesize(&req).await.unwrap();
    assert!(
        !again.cache_hit,
        "an entry with non-finite samples is unusable"
    );
    assert_eq!(again.audio, good.audio);
    assert!(
        client.synthesize(&req).await.unwrap().cache_hit,
        "entry rewritten"
    );
}

#[tokio::test]
async fn read_cache_false_still_writes_and_write_cache_false_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let req = mock("No cache reads.");
    let key = cache_key(&req).unwrap();

    let no_read = TtsClient::new(TtsOptions {
        cache_dir: Some(dir.path().to_path_buf()),
        read_cache: false,
        ..TtsOptions::default()
    });
    let a = no_read.synthesize(&req).await.unwrap();
    let b = no_read.synthesize(&req).await.unwrap();
    assert!(!a.cache_hit && !b.cache_hit);
    assert_eq!(no_read.cached(&req), None, "cached() honours read_cache");
    assert!(
        dir.path().join(format!("{key}.wav")).is_file(),
        "results are still written"
    );
    assert!(
        cached_client(dir.path())
            .synthesize(&req)
            .await
            .unwrap()
            .cache_hit
    );

    let other = tempfile::tempdir().unwrap();
    let no_write = TtsClient::new(TtsOptions {
        cache_dir: Some(other.path().to_path_buf()),
        write_cache: false,
        ..TtsOptions::default()
    });
    no_write.synthesize(&req).await.unwrap();
    assert!(entry_files(other.path()).is_empty());
}

#[tokio::test]
async fn synthesize_all_with_cache_and_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let client = cached_client(dir.path());
    client.synthesize(&mock("one")).await.unwrap();
    let results = client
        .synthesize_all(
            vec![mock("one"), mock("two"), mock("one"), mock("two")],
            |_| {},
        )
        .await;
    let results: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    assert!(results[0].cache_hit && results[2].cache_hit);
    assert!(!results[1].cache_hit && !results[3].cache_hit);
    assert_eq!(results[2].duplicate_of, Some(0));
    assert_eq!(results[3].duplicate_of, Some(1));
    assert_eq!(entry_files(dir.path()).len(), 4, "two entries");
}

#[tokio::test]
async fn cache_hits_make_no_http_requests() {
    let server = TestServer::start().await;
    server.route(
        "POST",
        "/v1/text-to-speech/voice/with-timestamps",
        vec![Reply::json(
            200,
            json!({"audio_base64": b64(&pcm16(2_400)),
            "alignment": {"characters": ["H", "i"], "character_start_times_seconds": [0.0, 0.05],
                          "character_end_times_seconds": [0.05, 0.1]}}),
        )],
    );
    let dir = tempfile::tempdir().unwrap();
    let client = TtsClient::new(TtsOptions {
        cache_dir: Some(dir.path().to_path_buf()),
        ..options()
    });
    let req = request(
        instance(ProviderType::Elevenlabs, Some(server.url())),
        voice(ProviderType::Elevenlabs, Some("voice"), json!({})),
        "Hi",
    );
    let fresh = client.synthesize(&req).await.unwrap();
    let hit = client.synthesize(&req).await.unwrap();
    assert!(hit.cache_hit);
    assert_eq!(hit.audio, fresh.audio);
    assert_eq!(hit.words, fresh.words);
    assert_eq!(server.requests().len(), 1);
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join(format!("{}.json", fresh.cache_key))).unwrap(),
    )
    .unwrap();
    assert_eq!(json["output_format"], "pcm_24000");
    assert_eq!(json["model"], "eleven_multilingual_v2");
}

#[cfg(unix)]
#[tokio::test]
async fn an_unwritable_cache_is_reported_apart_from_provider_notes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let unlock =
        || std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::write(locked.join("probe"), b"x").is_ok() {
        unlock(); // running as root: permissions are not enforced, nothing to test
        return;
    }
    let result = cached_client(&locked.join("sub"))
        .synthesize(&mock("Cache me if you can."))
        .await;
    unlock();
    let result = result.unwrap();
    assert!(!result.cache_hit);
    assert!(
        result.notes.is_empty(),
        "not a provider note: {:?}",
        result.notes
    );
    let reason = result
        .cache_write_error
        .expect("the failed write is reported");
    assert!(reason.contains("Permission denied"), "{reason}");
    // A writable cache reports nothing, and a cache hit wrote nothing.
    let client = cached_client(dir.path());
    assert_eq!(
        client
            .synthesize(&mock("Cache me."))
            .await
            .unwrap()
            .cache_write_error,
        None
    );
    let hit = client.synthesize(&mock("Cache me.")).await.unwrap();
    assert!(hit.cache_hit && hit.cache_write_error.is_none());
}

#[tokio::test]
async fn render_mock_is_exactly_what_synthesis_returns() {
    let client = TtsClient::new(TtsOptions::default());
    for (voice_id, settings, text) in [
        ("narrator", json!({}), "Hello there, world."),
        (
            "guest",
            json!({"wpm": 120, "speed": 1.3}),
            "Autonomous software development, explained.",
        ),
        ("x", json!({"foo": "bar"}), "Ready?"),
    ] {
        let req = request(
            instance(ProviderType::Mock, None),
            voice(ProviderType::Mock, Some(voice_id), settings),
            text,
        );
        let dry = render_mock(&req.provider, &req.voice, &req.text).expect("mock");
        let real = client.synthesize(&req).await.unwrap();
        assert_eq!(dry, real, "{text}");
        assert_eq!(dry.cache_key, cache_key(&req).unwrap());
    }
    let openai = instance(ProviderType::Openai, None);
    assert!(render_mock(&openai, &voice(ProviderType::Openai, None, json!({})), "Hi").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn projects_with_their_own_tts_script_never_share_cache_entries() {
    use std::os::unix::fs::PermissionsExt;
    // The reviewer's case: projA/tts.sh and projB/tts.sh behind the same template and text, one shared
    // cache directory (here the scripts are identical: only the directory they run in differs).
    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("shared-cache");
    let mut frames = Vec::new();
    for (name, length) in [("projA", 1_600), ("projB", 3_200)] {
        let project = root.path().join(name);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("fixture.wav"),
            support::wav_s16(16_000, &support::tone(length)),
        )
        .unwrap();
        let script = project.join("tts.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncp \"$(dirname \"$0\")/fixture.wav\" \"$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut provider = instance(ProviderType::Command, None);
        provider.command = Some(vec!["./tts.sh".into(), "{output}".into()]);
        let mut req = request(
            provider,
            voice(ProviderType::Command, Some("v"), json!({})),
            "The same words in both projects.",
        );
        req.base_dir = project;
        let result = cached_client(&cache).synthesize(&req).await.unwrap();
        assert!(
            !result.cache_hit,
            "{name} was served another project's audio"
        );
        frames.push(result.audio.frames());
    }
    assert_eq!(frames, [1_600, 3_200]);
}
