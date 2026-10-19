//! Every tool against the real engine (mock voices only: no API keys, no network).

mod common;

use std::path::Path;

use common::{
    TestClient, content_types, long_mock_timeline, mock_timeline, repo_root, result_chars, text,
};
use serde_json::{Value, json};

/// `file://` URI as the server builds it (percent-encoded path).
fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

fn assert_mirror(result: &Value) {
    let mirrored: Value = serde_json::from_str(&text(result, 1)).expect("second block is JSON");
    assert_eq!(
        mirrored, result["structuredContent"],
        "the JSON mirror equals structuredContent"
    );
}

#[tokio::test]
async fn validate_accepts_a_valid_inline_timeline() {
    let mut client = TestClient::start();
    client.initialize().await;
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_validate_timeline",
            json!({"yaml_content": mock_timeline("out.wav")}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let report = &result["structuredContent"];
    assert_eq!(report["valid"], true, "{report:#}");
    assert_eq!(report["summary"]["speech_cues"], 3);
    let summary = text(&result, 0);
    assert!(
        summary.starts_with("Timeline (inline yaml_content) is VALID"),
        "{summary}"
    );
    assert!(summary.contains("soniccraft_dry_run"), "{summary}");
    assert_mirror(&result);

    // JSON text works too (detected from the leading `{`), and so does the online credential check
    // (mock needs no key, so it makes no request).
    let json_timeline = json!({
        "version": "1.0",
        "tracks": [{"id": "v", "type": "speech", "provider": "mock", "sequence": [{"text": "Hi."}]}]
    })
    .to_string();
    let result = client
        .call_tool(
            json!(2),
            "soniccraft_validate_timeline",
            json!({"yaml_content": json_timeline, "check_credentials": true}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    assert_eq!(result["structuredContent"]["valid"], true, "{result:#}");
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn validate_reports_every_problem_without_failing_the_call() {
    let mut client = TestClient::start();
    client.initialize().await;
    let broken = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    provider: mock
    sequence:
      - txt: "Hello."
      - id: second
        text: "World."
        start_relative: { after_cue: frist }
  - id: bed
    type: music
    source: missing.wav
    fade_in: 500
"#;
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_validate_timeline",
            json!({"yaml_content": broken}),
        )
        .await;
    assert_eq!(
        result["isError"], false,
        "an invalid timeline is a successful call: {result:#}"
    );
    let report = &result["structuredContent"];
    assert_eq!(report["valid"], false);
    let diagnostics = report["diagnostics"].as_array().unwrap();
    let codes: Vec<&str> = diagnostics
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"unknown_field"), "{codes:?}");
    let typo = diagnostics
        .iter()
        .find(|d| d["path"] == "tracks[1].fade_in")
        .expect("fade_in diagnostic");
    assert!(
        typo["hint"].as_str().unwrap().contains("fade_in_ms"),
        "{typo:#}"
    );
    assert!(typo["line"].is_u64());

    let summary = text(&result, 0);
    assert!(summary.contains("is INVALID"), "{summary}");
    let line = summary
        .lines()
        .find(|l| l.starts_with("error[unknown_field] tracks[1].fade_in"))
        .unwrap_or_else(|| panic!("no fade_in line in {summary}"));
    assert!(
        line.contains(" — ") && line.contains("fade_in_ms"),
        "{line}"
    );
    assert!(summary.contains("validate_timeline again"), "{summary}");
    assert_mirror(&result);
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn timeline_files_resolve_against_base_dir_and_path_is_a_synonym() {
    let mut client = TestClient::start();
    client.initialize().await;
    let project = client.base().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("t.yaml"), mock_timeline("t.wav")).unwrap();

    let by_path = client
        .call_tool(
            json!(1),
            "soniccraft_validate_timeline",
            json!({"path": "project/t.yaml"}),
        )
        .await;
    assert_eq!(by_path["structuredContent"]["valid"], true, "{by_path:#}");
    let expected = project.join("t.yaml").display().to_string();
    assert!(
        text(&by_path, 0).contains(&expected),
        "{}",
        text(&by_path, 0)
    );

    let with_base = client
        .call_tool(
            json!(2),
            "soniccraft_dry_run",
            json!({"yaml_path": "t.yaml", "base_dir": project.display().to_string()}),
        )
        .await;
    assert_eq!(with_base["isError"], false, "{with_base:#}");
    assert_eq!(with_base["structuredContent"]["valid"], true);

    let both = client
        .call_tool(
            json!(3),
            "soniccraft_validate_timeline",
            json!({"yaml_path": "project/t.yaml", "path": "project/t.yaml"}),
        )
        .await;
    assert_eq!(
        both["isError"], false,
        "identical synonyms are fine: {both:#}"
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn dry_run_returns_the_layout_and_the_diagram() {
    let mut client = TestClient::start();
    client.initialize().await;
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_dry_run",
            json!({"yaml_content": mock_timeline("o.wav")}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let plan = &result["structuredContent"];
    assert_eq!(plan["valid"], true, "{plan:#}");
    let diagram = plan["diagram"].as_str().unwrap();
    assert!(!diagram.trim().is_empty());
    let summary = text(&result, 0);
    assert!(
        summary.contains(diagram.trim_end()),
        "the summary shows the diagram"
    );
    assert!(
        summary.contains("Dry run of (inline yaml_content): VALID"),
        "{summary}"
    );
    let items = plan["items"].as_array().unwrap();
    let cue_03 = items.iter().find(|i| i["id"] == "cue_03").expect("cue_03");
    let cue_02 = items.iter().find(|i| i["id"] == "cue_02").expect("cue_02");
    // cue_03 follows cue_01 implicitly and collides with cue_02, so the push policy moves it.
    assert!(cue_03["pushed_from_ms"].is_number(), "{cue_03:#}");
    assert!(cue_03["start_ms"].as_f64() >= cue_02["end_ms"].as_f64());
    assert!(
        summary.contains("Moved by the overlap policy: cue_03"),
        "{summary}"
    );
    assert_eq!(plan["synthesis"]["cues"], 3);
    assert_mirror(&result);

    let invalid = client
        .call_tool(
            json!(2),
            "soniccraft_dry_run",
            json!({"yaml_content": "version: \"1.0\"\n"}),
        )
        .await;
    assert_eq!(invalid["isError"], false, "{invalid:#}");
    assert_eq!(invalid["structuredContent"]["valid"], false);
    assert!(text(&invalid, 0).contains("INVALID"));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn render_writes_audio_and_side_files_with_absolute_paths() {
    let mut client = TestClient::start();
    client.initialize().await;
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_render",
            json!({
                "yaml_content": mock_timeline("ignored.wav"),
                "output_path": "renders/episode one.wav",
                "export_subtitles": true
            }),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let report = &result["structuredContent"];
    let output = &report["output"];
    let path = output["path"].as_str().unwrap();
    let expected = client.base().join("renders/episode one.wav");
    assert_eq!(
        Path::new(path),
        expected,
        "output_path resolves against base_dir"
    );
    let size = std::fs::metadata(path).expect("audio written").len();
    assert_eq!(output["bytes"].as_u64(), Some(size));
    assert_eq!(output["format"], "wav");

    let loudness = &report["loudness"];
    let integrated = loudness["integrated_lufs"].as_f64().expect("measured");
    let target = loudness["target_lufs"].as_f64().unwrap();
    assert!((integrated - target).abs() <= 0.5, "{loudness:#}");
    assert!(
        loudness["true_peak_dbtp"].as_f64().unwrap()
            <= loudness["true_peak_target_dbtp"].as_f64().unwrap() + 0.05
    );

    let subtitles = report["files"]["subtitles"].as_array().unwrap();
    let formats: Vec<&str> = subtitles
        .iter()
        .filter_map(|s| s["format"].as_str())
        .collect();
    assert_eq!(
        formats,
        ["vtt", "srt"],
        "export_subtitles: true = vtt + srt"
    );
    for sub in subtitles {
        let p = sub["path"].as_str().unwrap();
        assert!(Path::new(p).is_absolute() && Path::new(p).exists(), "{p}");
    }

    let links: Vec<&Value> = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["type"] == "resource_link")
        .collect();
    assert_eq!(links.len(), 3, "audio + vtt + srt: {links:#?}");
    assert_eq!(links[0]["uri"], file_uri(path));
    assert!(
        links[0]["uri"]
            .as_str()
            .unwrap()
            .contains("episode%20one.wav")
    );
    assert_eq!(links[0]["mimeType"], "audio/wav");
    assert_eq!(links[0]["name"], "episode one.wav");
    assert_eq!(links[0]["size"].as_u64(), Some(size));
    assert_eq!(links[1]["mimeType"], "text/vtt");
    assert!(
        result["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["type"] != "audio")
    );

    let summary = text(&result, 0);
    assert!(
        summary.starts_with(&format!("Rendered {path}\n")),
        "{summary}"
    );
    assert!(
        summary.contains("LUFS integrated (target -16.0 LUFS)"),
        "{summary}"
    );
    assert!(summary.contains("Subtitles: vtt "), "{summary}");
    assert_mirror(&result);

    // Same timeline again: everything comes from the TTS cache, unless no_cache is set.
    let again = client
        .call_tool(
            json!(2),
            "soniccraft_render",
            json!({"yaml_content": mock_timeline("x.wav"), "output_path": "renders/again.wav"}),
        )
        .await;
    assert_eq!(
        again["structuredContent"]["synthesis"]["cache_hits"], 3,
        "{again:#}"
    );
    let fresh = client
        .call_tool(
            json!(3),
            "soniccraft_render",
            json!({"yaml_content": mock_timeline("x.wav"), "output_path": "renders/fresh.wav", "no_cache": true}),
        )
        .await;
    assert_eq!(
        fresh["structuredContent"]["synthesis"]["cache_hits"], 0,
        "{fresh:#}"
    );
    assert_eq!(fresh["structuredContent"]["synthesis"]["synthesized"], 3);
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn render_of_an_example_file_uses_its_own_directory_for_assets() {
    let mut client = TestClient::start();
    client.initialize().await;
    let timeline = repo_root().join("examples/offline-demo/timeline.mock.yaml");
    let out = client.base().join("demo.wav");
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_render",
            json!({
                "yaml_path": timeline.display().to_string(),
                "output_path": out.display().to_string(),
                "export_subtitles": false
            }),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let report = &result["structuredContent"];
    assert!(out.exists());
    assert!(
        report["files"]
            .get("subtitles")
            .is_none_or(|s| s.as_array().is_some_and(Vec::is_empty))
    );
    // The example also exports a manifest, which is linked.
    let manifest = report["files"]["manifest"].as_str().expect("manifest");
    assert!(Path::new(manifest).exists());
    assert!(
        result["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "resource_link" && c["mimeType"] == "application/json")
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn inline_render_without_output_path_lands_in_base_dir() {
    let mut client = TestClient::start();
    client.initialize().await;
    let timeline = mock_timeline("named.wav");
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_render",
            json!({"yaml_content": timeline}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let path = result["structuredContent"]["output"]["path"]
        .as_str()
        .unwrap();
    assert_eq!(
        Path::new(path),
        client.base().join("named.wav"),
        "output.path is relative to base_dir"
    );

    let unnamed = "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    provider: mock\n    sequence:\n      - text: Hi.\n";
    let result = client
        .call_tool(
            json!(2),
            "soniccraft_render",
            json!({"yaml_content": unnamed}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let path = result["structuredContent"]["output"]["path"]
        .as_str()
        .unwrap();
    assert_eq!(Path::new(path).parent(), Some(client.base()), "{path}");
    assert!(Path::new(path).exists());
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn render_failures_are_tool_errors_with_diagnostics() {
    let mut client = TestClient::start();
    client.initialize().await;
    let missing_asset = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    provider: mock
    sequence:
      - text: "Hello."
  - id: bed
    type: music
    source: assets/does-not-exist.wav
"#;
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_render",
            json!({"yaml_content": missing_asset, "output_path": "fail.wav"}),
        )
        .await;
    assert_eq!(result["isError"], true, "{result:#}");
    let failure = &result["structuredContent"];
    assert!(failure["message"].is_string(), "{failure:#}");
    let codes: Vec<&str> = failure["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"asset_not_found"), "{codes:?}");
    let summary = text(&result, 0);
    assert!(summary.contains("FAILED"), "{summary}");
    assert!(summary.contains("error[asset_not_found]"), "{summary}");
    assert!(
        !client.base().join("fail.wav").exists(),
        "nothing is left behind"
    );
    assert_mirror(&result);

    let invalid = client
        .call_tool(
            json!(2),
            "soniccraft_render",
            json!({"yaml_content": "tracks: []"}),
        )
        .await;
    assert_eq!(invalid["isError"], true, "{invalid:#}");
    assert!(
        !invalid["structuredContent"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn providers_and_voices() {
    let mut client = TestClient::start();
    client.initialize().await;
    let result = client
        .call_tool(json!(1), "soniccraft_list_providers", json!({}))
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    let providers = result["structuredContent"]["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 8);
    let mock = providers
        .iter()
        .find(|p| p["name"] == "mock")
        .expect("mock");
    assert_eq!(mock["available"], true);
    let summary = text(&result, 0);
    assert!(
        summary.contains("usable now: ") && summary.contains("mock"),
        "{summary}"
    );
    assert!(summary.contains("- elevenlabs (ElevenLabs): "), "{summary}");
    assert_mirror(&result);

    let voices = client
        .call_tool(
            json!(2),
            "soniccraft_list_voices",
            json!({"provider": "mock"}),
        )
        .await;
    assert_eq!(voices["isError"], false, "{voices:#}");
    assert_eq!(voices["structuredContent"]["provider"], "mock");
    assert!(
        !voices["structuredContent"]["voices"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        text(&voices, 0).starts_with("mock: "),
        "{}",
        text(&voices, 0)
    );

    let unknown = client
        .call_tool(
            json!(3),
            "soniccraft_list_voices",
            json!({"provider": "elevenlab"}),
        )
        .await;
    assert_eq!(unknown["isError"], true, "{unknown:#}");
    let message = text(&unknown, 0);
    assert!(message.contains("built-in providers"), "{message}");
    assert!(message.contains("`elevenlabs`"), "did-you-mean: {message}");

    // One sentence that names the built-in providers once.
    let bogus = client
        .call_tool(
            json!(30),
            "soniccraft_list_voices",
            json!({"provider": "bogus"}),
        )
        .await;
    assert_eq!(bogus["isError"], true);
    let message = text(&bogus, 0);
    assert!(message.starts_with("Unknown provider `bogus`"), "{message}");
    assert_eq!(message.matches("macos_say").count(), 1, "{message}");
    assert_eq!(message.matches('.').count(), 1, "{message}");
    assert!(message.ends_with("mock."), "{message}");

    // Tool arguments accept case and separator variants of a provider name.
    let upper = client
        .call_tool(
            json!(31),
            "soniccraft_list_voices",
            json!({"provider": "MOCK"}),
        )
        .await;
    assert_eq!(upper["isError"], false, "{upper:#}");
    assert_eq!(upper["structuredContent"]["provider"], "mock");

    if cfg!(target_os = "macos") {
        let say = client
            .call_tool(
                json!(4),
                "soniccraft_list_voices",
                json!({"provider": "say"}),
            )
            .await;
        assert_eq!(say["isError"], false, "{say:#}");
        assert!(
            !say["structuredContent"]["voices"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_dry_runs_and_renders_stay_within_the_agent_budget() {
    // 400 cues and 399 pauses: about 460,000 characters of results before the size budget.
    let mut client = TestClient::start();
    client.initialize().await;
    std::fs::write(
        client.base().join("big.yaml"),
        long_mock_timeline(400, "big.wav"),
    )
    .unwrap();

    let plan = client
        .call_tool(
            json!(1),
            "soniccraft_dry_run",
            json!({"yaml_path": "big.yaml"}),
        )
        .await;
    assert_eq!(plan["isError"], false, "{plan:#}");
    assert!(
        result_chars(&plan) < 40_000,
        "{} chars",
        result_chars(&plan)
    );
    let report = &plan["structuredContent"];
    assert_eq!(report["valid"], true);
    assert_eq!(report["detail"], "summary");
    assert_eq!(report["items_total"], 799);
    assert_eq!(report["items_truncated"], true);
    let items = report["items"].as_array().unwrap();
    assert_eq!(items.len(), 20, "the first and last 10");
    assert_eq!(items[0]["id"], "c000");
    assert_eq!(items[19]["id"], "c399");
    assert_eq!(report["synthesis"]["cues"], 400);
    assert!(report["duration_ms"].as_f64().unwrap() > 1_000_000.0);
    assert!(
        report["full_report"]
            .as_str()
            .unwrap()
            .contains("soniccraft plan --json")
    );
    let summary = text(&plan, 0);
    assert!(summary.contains("Items (799 in total"), "{summary}");
    assert!(
        summary.contains("talk "),
        "the track lanes are shown: {summary}"
    );
    assert!(summary.lines().count() < 80, "{summary}");
    assert_mirror(&plan);

    // `full` returns every item and the whole diagram.
    let full = client
        .call_tool(
            json!(2),
            "soniccraft_dry_run",
            json!({"yaml_path": "big.yaml", "detail": "full"}),
        )
        .await;
    let report = &full["structuredContent"];
    assert_eq!(report["items"].as_array().unwrap().len(), 799);
    assert!(report.get("items_truncated").is_none());
    assert!(report["diagram"].as_str().unwrap().contains("400 cues:"));
    assert_mirror(&full);

    // `summary` is compact even for a small timeline.
    let small = client
        .call_tool(
            json!(3),
            "soniccraft_dry_run",
            json!({"yaml_content": mock_timeline("s.wav"), "detail": "summary"}),
        )
        .await;
    assert_eq!(small["structuredContent"]["detail"], "summary");
    assert_eq!(small["structuredContent"]["items_truncated"], false);

    let render = client
        .call_tool(
            json!(4),
            "soniccraft_render",
            json!({"yaml_path": "big.yaml", "export_subtitles": false}),
        )
        .await;
    assert_eq!(render["isError"], false, "{render:#}");
    assert!(
        result_chars(&render) < 40_000,
        "{} chars",
        result_chars(&render)
    );
    let report = &render["structuredContent"];
    let path = report["output"]["path"].as_str().unwrap();
    assert!(Path::new(path).exists(), "{path}");
    assert!(
        report["loudness"]["integrated_lufs"].is_number(),
        "{report:#}"
    );
    assert_eq!(report["items_total"], 799);
    assert_eq!(report["items_truncated"], true);
    assert!(
        report["full_report"]
            .as_str()
            .unwrap()
            .contains("detail: \"full\""),
        "{report:#}"
    );
    let summary = text(&render, 0);
    assert!(
        summary.starts_with(&format!("Rendered {path}\n")),
        "{summary}"
    );
    assert!(summary.contains("LUFS integrated"), "{summary}");
    assert_mirror(&render);
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn render_results_follow_the_protocol_era() {
    for (version, links) in [
        ("2024-11-05", false),
        ("2025-03-26", false),
        ("2025-06-18", true),
        ("2025-11-25", true),
    ] {
        let mut client = TestClient::start();
        client.initialize_with(version).await;
        let result = client
            .call_tool(
                json!(1),
                "soniccraft_render",
                json!({"yaml_content": mock_timeline("era.wav"), "export_subtitles": true}),
            )
            .await;
        assert_eq!(result["isError"], false, "{version}: {result:#}");
        let types = content_types(&result);
        if links {
            assert_eq!(
                types,
                [
                    "text",
                    "text",
                    "resource_link",
                    "resource_link",
                    "resource_link"
                ],
                "{version}"
            );
        } else {
            // 2025-03-26 defines text, image, audio and embedded resources only: strict clients reject
            // unknown content types, so the files come as text.
            assert_eq!(types, ["text", "text", "text"], "{version}");
            let files = text(&result, 2);
            let audio = result["structuredContent"]["output"]["path"]
                .as_str()
                .unwrap();
            assert!(files.starts_with("Files written:\n"), "{files}");
            assert!(files.contains(audio), "{files}");
            assert!(files.contains("Subtitles (vtt)") && files.contains("Subtitles (srt)"));
        }
        client.shutdown().await.expect("clean shutdown");
    }
}

#[tokio::test]
async fn render_format_overrides_the_timeline_format() {
    let mut client = TestClient::start();
    client.initialize().await;
    let timeline = r#"version: "1.0"
output: { path: timeline.wav, format: wav, bit_depth: 16 }
tracks:
  - id: v
    type: speech
    provider: mock
    voice_id: narrator
    sequence:
      - text: "Hello from SonicCraft."
"#;
    // Without `format`, the timeline's `format: wav` wins over the .flac extension, and the summary says
    // how to fix it.
    let mismatch = client
        .call_tool(
            json!(1),
            "soniccraft_render",
            json!({"yaml_content": timeline, "output_path": "out/demo.flac"}),
        )
        .await;
    assert_eq!(mismatch["isError"], false, "{mismatch:#}");
    assert_eq!(mismatch["structuredContent"]["output"]["format"], "wav");
    let summary = text(&mismatch, 0);
    assert!(
        summary.contains("call soniccraft_render again with `format: \"flac\"`"),
        "{summary}"
    );

    for (id, format) in [(2, "flac"), (3, "FLAC"), (4, ".flac")] {
        let result = client
            .call_tool(
                json!(id),
                "soniccraft_render",
                json!({"yaml_content": timeline, "output_path": "out/demo.flac", "format": format}),
            )
            .await;
        assert_eq!(result["isError"], false, "{format}: {result:#}");
        let output = &result["structuredContent"]["output"];
        assert_eq!(output["format"], "flac", "{format}");
        let bytes = std::fs::read(output["path"].as_str().unwrap()).unwrap();
        assert_eq!(&bytes[..4], b"fLaC", "{format}: FLAC data");
        assert!(!text(&result, 0).contains("Note: the file holds"));
    }

    let bad = client
        .call_tool(
            json!(5),
            "soniccraft_render",
            json!({"yaml_content": timeline, "format": "aiff"}),
        )
        .await;
    assert_eq!(bad["isError"], true, "{bad:#}");
    let message = text(&bad, 0);
    assert!(
        message.starts_with("Invalid argument `format` for `soniccraft_render`"),
        "{message}"
    );
    assert!(message.contains("wav, mp3, flac"), "{message}");
    assert!(
        !client.base().join("timeline.wav").exists(),
        "nothing rendered"
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn detail_values_are_checked() {
    let mut client = TestClient::start();
    client.initialize().await;
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_dry_run",
            json!({"yaml_content": mock_timeline("d.wav"), "detail": "ful"}),
        )
        .await;
    assert_eq!(result["isError"], true, "{result:#}");
    let message = text(&result, 0);
    assert!(
        message.contains("`detail`") && message.contains("did you mean \"full\"?"),
        "{message}"
    );
    for detail in ["AUTO", "Full", " summary "] {
        let ok = client
            .call_tool(
                json!(2),
                "soniccraft_validate_timeline",
                json!({"yaml_content": mock_timeline("d.wav"), "detail": detail}),
            )
            .await;
        assert_eq!(ok["isError"], false, "{detail}: {ok:#}");
    }
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn oversized_and_special_timeline_files_fail_fast() {
    let mut client = TestClient::start();
    client.initialize().await;
    // One byte over the 5 MiB document limit.
    let big = client.base().join("big.yaml");
    let mut text_5mib = "x".repeat(5 * 1024 * 1024 + 1);
    text_5mib.replace_range(..7, "title: ");
    std::fs::write(&big, text_5mib).unwrap();
    let started = std::time::Instant::now();
    let result = client
        .call_tool(
            json!(1),
            "soniccraft_validate_timeline",
            json!({"yaml_path": "big.yaml"}),
        )
        .await;
    assert_eq!(result["isError"], true, "{result:#}");
    let message = text(&result, 0);
    assert!(
        message.contains("at most 5 MiB (5242880 bytes)"),
        "{message}"
    );
    assert!(message.contains("is 5242881 bytes"), "{message}");

    #[cfg(unix)]
    {
        for (id, path, kind) in [(2, "/dev/zero", "a character device")] {
            let result = client
                .call_tool(json!(id), "soniccraft_dry_run", json!({"yaml_path": path}))
                .await;
            assert_eq!(result["isError"], true, "{result:#}");
            let message = text(&result, 0);
            assert!(message.contains(kind), "{message}");
            assert!(message.contains("regular timeline file"), "{message}");
        }
        let fifo = client.base().join("pipe.yaml");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            let result = client
                .call_tool(
                    json!(3),
                    "soniccraft_render",
                    json!({"yaml_path": fifo.display().to_string()}),
                )
                .await;
            assert_eq!(result["isError"], true, "{result:#}");
            assert!(text(&result, 0).contains("a FIFO"), "{}", text(&result, 0));
        }
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn a_timelines_own_output_path_stays_inside_the_base_dir() {
    let mut client = TestClient::start();
    client.initialize().await;
    let project = client.base().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let base_dir = project.display().to_string();
    let escaping = r#"version: "1.0"
output: { path: ../escape.wav }
tracks:
  - id: v
    type: speech
    provider: mock
    voice_id: narrator
    sequence:
      - text: "Hello."
"#;
    let refused = client
        .call_tool(
            json!(1),
            "soniccraft_render",
            json!({"yaml_content": escaping, "base_dir": base_dir}),
        )
        .await;
    assert_eq!(refused["isError"], true, "{refused:#}");
    let diagnostics = refused["structuredContent"]["diagnostics"]
        .as_array()
        .unwrap();
    let error = diagnostics
        .iter()
        .find(|d| d["path"] == "output.path" && d["severity"] == "error")
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    assert_eq!(error["code"], "invalid_value", "{error:#}");
    assert!(
        error["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("output_path"),
        "{error:#}"
    );
    assert!(!client.base().join("escape.wav").exists());

    // An explicit output_path is the caller's choice.
    let allowed = client
        .call_tool(
            json!(2),
            "soniccraft_render",
            json!({"yaml_content": escaping, "base_dir": base_dir, "output_path": "../explicit.wav"}),
        )
        .await;
    assert_eq!(allowed["isError"], false, "{allowed:#}");
    assert!(client.base().join("explicit.wav").exists());
    client.shutdown().await.expect("clean shutdown");
}
