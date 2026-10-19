//! Protocol behaviour over in-memory pipes: both lifecycle eras, framing, error codes, tools/list.

mod common;

use std::time::Duration;

use common::{LEGACY_LATEST, TestClient, modern_meta, text};
use serde_json::{Value, json};

const LEGACY_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

#[tokio::test]
async fn initialize_echoes_every_supported_version() {
    for version in LEGACY_VERSIONS {
        let mut client = TestClient::start();
        let response = client.initialize_with(version).await;
        let result = &response["result"];
        assert_eq!(result["protocolVersion"], version, "{response:#}");
        assert_eq!(result["serverInfo"]["name"], "soniccraft");
        assert_eq!(result["serverInfo"]["title"], "SonicCraft");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(result["capabilities"]["tools"].is_object(), "{result:#}");
        let instructions = result["instructions"].as_str().expect("instructions");
        assert!(instructions.len() <= 4096 && instructions.starts_with("SonicCraft"));
        assert!(instructions.contains(&client.base().display().to_string()));
        assert!(
            result.get("resultType").is_none(),
            "legacy results carry no resultType"
        );
        client.shutdown().await.expect("clean shutdown");
    }
}

#[tokio::test]
async fn unknown_versions_negotiate_the_newest_handshake_version() {
    for requested in ["2099-01-01", "1.0.0", "2026-07-28", ""] {
        let mut client = TestClient::start();
        let response = client.initialize_with(requested).await;
        assert_eq!(
            response["result"]["protocolVersion"], LEGACY_LATEST,
            "requested {requested:?}: {response:#}"
        );
        let tools = client.request(json!(1), "tools/list", None).await;
        assert_eq!(tools["result"]["tools"].as_array().map(Vec::len), Some(6));
        client.shutdown().await.expect("clean shutdown");
    }
}

#[tokio::test]
async fn initialize_without_a_version_is_invalid_params_and_recoverable() {
    let mut client = TestClient::start();
    let response = client
        .request(
            json!(0),
            "initialize",
            Some(json!({"capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})),
        )
        .await;
    assert_eq!(response["error"]["code"], -32602, "{response:#}");
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.contains("protocolVersion"), "{message}");
    assert!(
        response["error"]["data"]["supported"]
            .as_array()
            .is_some_and(|v| v.contains(&json!("2025-11-25"))),
        "{response:#}"
    );
    // The connection stays usable: a correct handshake still works.
    let ok = client.initialize_with("2025-11-25").await;
    assert_eq!(ok["result"]["protocolVersion"], "2025-11-25");
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn ping_works_before_and_after_initialize() {
    let mut client = TestClient::start();
    let early = client.request(json!("early"), "ping", None).await;
    assert_eq!(early["result"], json!({}), "{early:#}");
    client.initialize().await;
    let late = client.request(json!(2), "ping", None).await;
    assert_eq!(late["result"], json!({}));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn modern_discover_and_stateless_requests() {
    let mut client = TestClient::start();
    let discover = client
        .request(
            json!("server-discover-probe-1"),
            "server/discover",
            Some(json!({"_meta": modern_meta()})),
        )
        .await;
    let result = &discover["result"];
    assert_eq!(result["resultType"], "complete", "{discover:#}");
    let versions: Vec<&str> = result["supportedVersions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for v in LEGACY_VERSIONS.iter().chain(&["2026-07-28"]) {
        assert!(versions.contains(v), "{v} missing from {versions:?}");
    }
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["ttlMs"].is_u64() && result["cacheScope"].is_string());
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "soniccraft"
    );
    assert!(
        result["instructions"]
            .as_str()
            .unwrap()
            .contains("soniccraft_render")
    );

    let list = client
        .request(
            json!(1),
            "tools/list",
            Some(json!({"_meta": modern_meta()})),
        )
        .await;
    assert_eq!(list["result"]["resultType"], "complete", "{list:#}");
    assert!(list["result"]["ttlMs"].is_u64() && list["result"]["cacheScope"].is_string());
    assert_eq!(list["result"]["tools"].as_array().map(Vec::len), Some(6));

    let call = client
        .request(
            json!(2),
            "tools/call",
            Some(json!({"name": "soniccraft_get_schema", "arguments": {}, "_meta": modern_meta()})),
        )
        .await;
    assert_eq!(call["result"]["resultType"], "complete", "{call:#}");
    assert_eq!(call["result"]["isError"], false);

    // ping was removed in 2026-07-28.
    let ping = client
        .request(json!(3), "ping", Some(json!({"_meta": modern_meta()})))
        .await;
    assert_eq!(ping["error"]["code"], -32601, "{ping:#}");

    // A request that omits the per-request metadata is rejected, not ignored.
    let bare = client.request(json!(4), "tools/list", None).await;
    assert_eq!(bare["error"]["code"], -32602, "{bare:#}");

    // Unknown methods are -32601 in this era too.
    let unknown = client
        .request(json!(5), "foo/bar", Some(json!({"_meta": modern_meta()})))
        .await;
    assert_eq!(unknown["error"]["code"], -32601, "{unknown:#}");
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn oversized_lines_are_rejected_and_the_connection_survives() {
    let mut client = TestClient::start();
    client.initialize().await;
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"soniccraft_validate_timeline","arguments":{{"yaml_content":"{}"}}}}}}"#,
        "x".repeat(17 * 1024 * 1024)
    );
    client.send_line(&huge).await;
    let error = client
        .next_message(common::TIMEOUT)
        .await
        .expect("an error");
    assert_eq!(error["error"]["code"], -32600, "{error:#}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too large")
    );
    let ping = client.request(json!(2), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn modern_requests_need_meta_and_a_supported_version() {
    let mut client = TestClient::start();
    let bare = client.request(json!(1), "tools/list", None).await;
    assert_eq!(bare["error"]["code"], -32602, "{bare:#}");
    assert!(bare["error"]["message"].as_str().unwrap().contains("_meta"));

    let mut meta = modern_meta();
    meta["io.modelcontextprotocol/protocolVersion"] = json!("1900-01-01");
    let old = client
        .request(json!(2), "tools/list", Some(json!({"_meta": meta})))
        .await;
    assert_eq!(old["error"]["code"], -32022, "{old:#}");
    assert_eq!(old["error"]["data"]["requested"], "1900-01-01");
    assert!(old["error"]["data"]["supported"].is_array());

    // Neither error locked the era: the legacy handshake still works.
    client.initialize().await;
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn discover_then_initialize_falls_back_to_legacy() {
    let mut client = TestClient::start();
    let discover = client
        .request(
            json!(0),
            "server/discover",
            Some(json!({"_meta": modern_meta()})),
        )
        .await;
    assert!(discover.get("result").is_some(), "{discover:#}");
    let init = client.initialize_with("2025-06-18").await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let ping = client.request(json!(5), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn malformed_json_is_a_parse_error_and_the_connection_survives() {
    let mut client = TestClient::start();
    client.send_line("{this is not json").await;
    let error = client
        .next_message(common::TIMEOUT)
        .await
        .expect("parse error");
    assert_eq!(error["error"]["code"], -32700, "{error:#}");
    assert_eq!(error["id"], Value::Null);
    assert_eq!(error["jsonrpc"], "2.0");

    client.initialize().await;
    client.send_line("{\"jsonrpc\":\"2.0\",\"id\":9,").await;
    let error = client
        .next_message(common::TIMEOUT)
        .await
        .expect("parse error");
    assert_eq!(error["error"]["code"], -32700, "{error:#}");
    let ping = client.request(json!(10), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn invalid_requests_are_answered_with_their_id() {
    let mut client = TestClient::start();
    client.initialize().await;
    client
        .send_line(r#"{"jsonrpc":"1.0","id":5,"method":"ping"}"#)
        .await;
    let error = client.response(json!(5)).await;
    assert_eq!(error["error"]["code"], -32600, "{error:#}");

    client
        .send_line(r#"[{"jsonrpc":"2.0","id":6,"method":"ping"}]"#)
        .await;
    let batch = client
        .next_message(common::TIMEOUT)
        .await
        .expect("batch error");
    assert_eq!(batch["error"]["code"], -32600, "{batch:#}");
    assert!(
        batch["error"]["message"]
            .as_str()
            .unwrap()
            .contains("batch")
    );

    // Malformed notifications are never answered.
    client
        .send_line(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":7}"#)
        .await;
    let ping = client.request(json!(8), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    assert!(client.backlog.is_empty(), "{:#?}", client.backlog);
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn unknown_methods_and_bad_params_have_precise_codes() {
    let mut client = TestClient::start();
    client.initialize().await;
    let unknown = client.request(json!(1), "foo/bar", Some(json!({}))).await;
    assert_eq!(unknown["error"]["code"], -32601, "{unknown:#}");
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("foo/bar")
    );

    let no_params = client.request(json!(2), "tools/call", None).await;
    assert_eq!(no_params["error"]["code"], -32602, "{no_params:#}");
    let bad_name = client
        .request(json!(3), "tools/call", Some(json!({"name": 42})))
        .await;
    assert_eq!(bad_name["error"]["code"], -32602, "{bad_name:#}");

    let unknown_tool = client
        .request(
            json!(4),
            "tools/call",
            Some(json!({"name": "render_timeline", "arguments": {}})),
        )
        .await;
    assert_eq!(unknown_tool["error"]["code"], -32602, "{unknown_tool:#}");
    let message = unknown_tool["error"]["message"].as_str().unwrap();
    assert!(message.contains("soniccraft_render"), "{message}");

    // An unknown argument is a tool execution error the model can correct (SEP-1303), not -32602.
    let unknown_arg = client
        .call_tool(
            json!(5),
            "soniccraft_render",
            json!({"timeline_path": "x.yaml"}),
        )
        .await;
    assert_eq!(unknown_arg["isError"], true, "{unknown_arg:#}");
    let message = text(&unknown_arg, 0);
    assert!(message.contains("Did you mean `yaml_path`"), "{message}");
    assert!(
        message.contains("Valid arguments: yaml_content, yaml_path, path, base_dir"),
        "{message}"
    );
    let details = &unknown_arg["structuredContent"];
    assert_eq!(details["error"], "unknown_argument");
    assert_eq!(details["unknown"], json!(["timeline_path"]));
    assert_eq!(details["suggestions"]["timeline_path"], "yaml_path");
    assert!(
        details["valid"]
            .as_array()
            .unwrap()
            .contains(&json!("yaml_path"))
    );

    let cursor = client
        .request(json!(6), "tools/list", Some(json!({"cursor": "page-2"})))
        .await;
    assert_eq!(cursor["error"]["code"], -32602, "{cursor:#}");
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn bad_argument_values_are_tool_errors() {
    let mut client = TestClient::start();
    client.initialize().await;
    let cases = [
        (json!({}), "Missing timeline"),
        (
            json!({"yaml_content": "version: 1", "yaml_path": "a.yaml"}),
            "exactly one",
        ),
        (json!({"yaml_path": "a.yaml", "path": "b.yaml"}), "synonym"),
        (json!({"yaml_path": "missing.yaml"}), "not found"),
        (json!({"yaml_path": 42}), "`yaml_path`"),
        (
            json!({"yaml_content": "x: 1", "base_dir": "no/such/dir"}),
            "does not exist",
        ),
        (
            json!({"yaml_content": "x: 1", "check_credentials": "yes"}),
            "`check_credentials`",
        ),
    ];
    for (i, (args, expected)) in cases.into_iter().enumerate() {
        let result = client
            .call_tool(json!(100 + i), "soniccraft_validate_timeline", args.clone())
            .await;
        assert_eq!(result["isError"], true, "{args}: {result:#}");
        let message = text(&result, 0);
        assert!(message.contains(expected), "{args}: {message}");
    }
    let voices = client
        .call_tool(
            json!(200),
            "soniccraft_list_voices",
            json!({"provider": "  "}),
        )
        .await;
    assert_eq!(voices["isError"], true);
    assert!(text(&voices, 0).contains("elevenlabs"));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn stray_messages_before_initialize_are_ignored() {
    let mut client = TestClient::start();
    client
        .notify("notifications/cancelled", Some(json!({"requestId": 99})))
        .await;
    client.notify("notifications/initialized", None).await;
    client
        .send_line(r#"{"jsonrpc":"2.0","id":"stray","result":{}}"#)
        .await;
    client.initialize().await;
    let ping = client.request(json!(1), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    assert!(client.backlog.is_empty(), "{:#?}", client.backlog);
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn immediate_eof_is_a_clean_shutdown() {
    let client = TestClient::start();
    client
        .shutdown()
        .await
        .expect("EOF before initialize is clean");

    let mut client = TestClient::start();
    client.initialize().await;
    client
        .shutdown()
        .await
        .expect("EOF after initialize is clean");

    // A final message without a trailing newline is still answered before shutdown.
    let mut client = TestClient::start();
    client.initialize().await;
    client
        .send_bytes(br#"{"jsonrpc":"2.0","id":"last","method":"ping"}"#)
        .await;
    client.close_input();
    let response = client.response(json!("last")).await;
    assert_eq!(response["result"], json!({}), "{response:#}");
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn tools_list_is_deterministic_and_well_formed() {
    let mut client = TestClient::start();
    client.initialize().await;
    let first = client.request(json!(1), "tools/list", None).await;
    let second = client.request(json!(2), "tools/list", None).await;
    assert_eq!(
        first["result"], second["result"],
        "tools/list must not vary"
    );
    assert!(first["result"].get("nextCursor").is_none());
    let tools = first["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, soniccraft_mcp::TOOL_NAMES);
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(tool["title"].is_string(), "{name}");
        let description = tool["description"].as_str().unwrap();
        assert!(
            description.len() > 200 && description.len() <= 2048,
            "{name}: {} chars",
            description.len()
        );
        let annotations = &tool["annotations"];
        for hint in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(annotations[hint].is_boolean(), "{name}.{hint}");
        }
        let schema = &tool["inputSchema"];
        jsonschema::draft202012::meta::validate(schema)
            .unwrap_or_else(|e| panic!("{name}: invalid JSON Schema: {e}"));
        assert_eq!(schema["type"], "object", "{name}");
        let compiled = jsonschema::draft202012::new(schema).expect("schema compiles");
        assert!(
            !compiled.is_valid(&json!({"definitely_not_an_argument": 1})),
            "{name}"
        );
        for (prop, spec) in schema["properties"].as_object().unwrap() {
            assert!(
                spec["description"].is_string(),
                "{name}.{prop} undocumented"
            );
            assert!(
                prop.len() <= 64 && prop.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "{name}.{prop}"
            );
        }
        let text = schema.to_string();
        assert!(
            !text.contains("$ref") && !text.contains("$defs"),
            "{name}: {text}"
        );
    }
    let render = tools
        .iter()
        .find(|t| t["name"] == "soniccraft_render")
        .unwrap();
    let validator = jsonschema::draft202012::new(&render["inputSchema"]).unwrap();
    assert!(validator.is_valid(&json!({"yaml_path": "/abs/t.yaml", "export_subtitles": true})));
    assert!(
        validator.is_valid(&json!({"path": "t.yaml", "output_path": "out.mp3", "no_cache": false}))
    );
    assert!(!validator.is_valid(&json!({"yaml_path": 3})));
    assert_eq!(render["annotations"]["readOnlyHint"], false);
    assert_eq!(render["annotations"]["destructiveHint"], true);
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn every_output_line_is_one_compact_json_message() {
    let mut client = TestClient::start();
    client.initialize().await;
    // The schema result carries multi-line Markdown and a large JSON document.
    let result = client
        .call_tool(json!(1), "soniccraft_get_schema", json!({}))
        .await;
    assert_eq!(result["isError"], false);
    client.request(json!(2), "tools/list", None).await;
    for line in &client.raw_lines {
        assert!(!line.contains('\n') && !line.contains('\r'));
        let value: Value = serde_json::from_str(line).expect("each line is JSON");
        assert_eq!(value["jsonrpc"], "2.0");
        assert!(line.len() < 1024 * 1024, "{} bytes", line.len());
    }
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn get_schema_returns_quick_reference_and_schema() {
    let mut client = TestClient::start();
    client.initialize().await;
    let result = client
        .call_tool(json!(1), "soniccraft_get_schema", json!({}))
        .await;
    assert_eq!(result["isError"], false, "{result:#}");
    // By default the quick reference alone: the JSON Schema would push the result past what agents keep
    // inline (Claude Code: about 50,000 characters).
    assert!(
        common::result_chars(&result) < 40_000,
        "{} chars",
        common::result_chars(&result)
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["dsl_version"], "1.0");
    assert!(structured.get("schema").is_none(), "{structured:#}");
    assert_eq!(
        structured["quick_reference"],
        soniccraft_mcp::DSL_QUICK_REFERENCE
    );
    assert!(text(&result, 0).starts_with("# SonicCraft timeline DSL"));
    assert!(
        text(&result, 1).contains("detail: \"full\""),
        "{}",
        text(&result, 1)
    );

    let full = client
        .call_tool(json!(3), "soniccraft_get_schema", json!({"detail": "full"}))
        .await;
    assert_eq!(full["isError"], false, "{full:#}");
    assert_eq!(
        full["structuredContent"]["schema"],
        soniccraft_engine::timeline_schema()
    );
    let mirrored: Value = serde_json::from_str(&text(&full, 1)).expect("schema JSON");
    assert_eq!(mirrored, soniccraft_engine::timeline_schema());

    let extra = client
        .call_tool(json!(2), "soniccraft_get_schema", json!({"format": "json"}))
        .await;
    assert_eq!(extra["isError"], true, "{extra:#}");
    assert!(
        text(&extra, 0).starts_with("Unknown argument `format` for `soniccraft_get_schema`."),
        "{}",
        text(&extra, 0)
    );
    let none = client
        .call_tool(json!(4), "soniccraft_list_providers", json!({"x": 1}))
        .await;
    assert_eq!(none["isError"], true, "{none:#}");
    assert!(text(&none, 0).contains("takes no arguments; remove `x`"));
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn requests_are_answered_out_of_order() {
    let mut client = TestClient::start();
    client.initialize().await;
    for id in 1..=20 {
        client.send_request(json!(id), "ping", None).await;
    }
    for id in (1..=20).rev() {
        let response = client
            .response_within(json!(id), Duration::from_secs(10))
            .await;
        assert_eq!(response["result"], json!({}));
    }
    client.shutdown().await.expect("clean shutdown");
}

/// Raw request lines whose `id` is not a string or an integer.
const UNUSABLE_IDS: [&str; 5] = ["null", "1.5", "true", "[1]", "{\"a\":1}"];

async fn assert_invalid_ids_are_answered(client: &mut TestClient, meta: Option<Value>) {
    for id in UNUSABLE_IDS {
        let mut request = json!({"jsonrpc": "2.0", "method": "tools/list"});
        if let Some(meta) = &meta {
            request["params"] = json!({"_meta": meta});
        }
        let line = request
            .to_string()
            .replacen('{', &format!("{{\"id\":{id},"), 1);
        client.send_line(&line).await;
        let reply = client
            .next_message(common::TIMEOUT)
            .await
            .unwrap_or_else(|| panic!("no reply to {line}"));
        assert_eq!(reply["error"]["code"], -32600, "{line}: {reply:#}");
        assert_eq!(
            reply["error"]["message"],
            "Invalid Request: `id` must be a string or an integer"
        );
        assert_eq!(reply["id"], Value::Null, "{reply:#}");
    }
}

#[tokio::test]
async fn requests_with_unusable_ids_get_invalid_request_in_every_state() {
    // Before initialize, after a legacy initialize, and in the stateless 2026-07-28 revision.
    let mut client = TestClient::start();
    assert_invalid_ids_are_answered(&mut client, None).await;
    client.initialize().await;
    assert_invalid_ids_are_answered(&mut client, None).await;
    let ping = client.request(json!(2), "ping", None).await;
    assert_eq!(
        ping["result"],
        json!({}),
        "later requests are still answered"
    );
    client.shutdown().await.expect("clean shutdown");

    let mut client = TestClient::start();
    assert_invalid_ids_are_answered(&mut client, Some(modern_meta())).await;
    let list = client
        .request(
            json!(3),
            "tools/list",
            Some(json!({"_meta": modern_meta()})),
        )
        .await;
    assert!(list["result"]["tools"].is_array(), "{list:#}");
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn positional_params_are_invalid_params() {
    let mut client = TestClient::start();
    client.initialize().await;
    client
        .send_line(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":[1,2]}"#)
        .await;
    let reply = client.response(json!(9)).await;
    assert_eq!(reply["error"]["code"], -32602, "{reply:#}");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("`params` must be an object") && message.contains("got an array"),
        "{message}"
    );
    assert!(
        !message.contains("untagged enum"),
        "no serde internals: {message}"
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn modern_results_carry_the_server_identity() {
    let mut client = TestClient::start();
    let params = |extra: Value| {
        let mut params = json!({"_meta": modern_meta()});
        for (key, value) in extra.as_object().unwrap() {
            params[key] = value.clone();
        }
        Some(params)
    };
    let requests = [
        ("tools/list", json!({})),
        (
            "tools/call",
            json!({"name": "soniccraft_list_providers", "arguments": {}}),
        ),
        (
            "tools/call",
            json!({"name": "soniccraft_validate_timeline", "arguments": {"yaml_contnet": "x"}}),
        ),
        ("prompts/list", json!({})),
        ("resources/list", json!({})),
        ("resources/templates/list", json!({})),
        ("server/discover", json!({})),
    ];
    for (i, (method, extra)) in requests.iter().enumerate() {
        let response = client
            .request(json!(i), method, params(extra.clone()))
            .await;
        let info = &response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];
        assert_eq!(info["name"], "soniccraft", "{method}: {response:#}");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"), "{method}");
        assert_eq!(response["result"]["resultType"], "complete", "{method}");
    }
    client.shutdown().await.expect("clean shutdown");

    // Legacy sessions keep the legacy shape.
    let mut client = TestClient::start();
    client.initialize_with(LEGACY_LATEST).await;
    for (i, (method, extra)) in requests[..4].iter().enumerate() {
        let response = client
            .request(json!(i + 1), method, Some(extra.clone()))
            .await;
        assert!(
            response["result"].get("_meta").is_none(),
            "{method}: {response:#}"
        );
    }
    client.shutdown().await.expect("clean shutdown");
}
