//! MCP smoke tests through the binaries: `soniccraft mcp` and the zero-argument `soniccraft-mcp` answer
//! the `initialize` handshake and `tools/list`, write nothing but protocol lines to stdout, and exit
//! cleanly when stdin closes.

mod common;

use std::time::Duration;

use serde_json::json;

use common::{McpSession, bin, mcp_bin, temp_dir};

const TIMEOUT: Duration = Duration::from_secs(20);

const TOOLS: [&str; 6] = [
    "soniccraft_validate_timeline",
    "soniccraft_dry_run",
    "soniccraft_render",
    "soniccraft_get_schema",
    "soniccraft_list_providers",
    "soniccraft_list_voices",
];

fn handshake(program: &str, args: &[&str]) {
    let dir = temp_dir();
    let mut session = McpSession::spawn(program, dir.path(), args);
    session.send(&json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "soniccraft-cli-test", "version": "0"}
        }
    }));
    let init = session.response(0, TIMEOUT);
    let result = &init["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18", "{init}");
    assert_eq!(result["serverInfo"]["name"], "soniccraft", "{init}");
    assert!(result["capabilities"]["tools"].is_object(), "{init}");

    session.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    session.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let list = session.response(1, TIMEOUT);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result: {list}"))
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for tool in TOOLS {
        assert!(names.contains(&tool), "missing {tool} in {names:?}");
    }

    session.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    let ping = session.response(2, TIMEOUT);
    assert!(ping["result"].is_object(), "{ping}");

    let (status, stderr) = session.finish(TIMEOUT);
    assert_eq!(status, 0, "exit status after stdin EOF; stderr:\n{stderr}");
    assert!(
        !stderr.contains('\x1b'),
        "ANSI codes in MCP logs: {stderr:?}"
    );
}

#[test]
fn soniccraft_mcp_subcommand_handshake() {
    handshake(bin(), &["mcp"]);
}

#[test]
fn soniccraft_mcp_with_base_dir_and_verbose_logging() {
    let base = temp_dir();
    let base_dir = base.path().to_string_lossy().into_owned();
    handshake(
        bin(),
        &["mcp", "--base-dir", &base_dir, "-vv", "--color", "always"],
    );
}

#[test]
fn zero_argument_launcher_handshake() {
    handshake(mcp_bin(), &[]);
}

#[test]
fn modern_era_discovery() {
    let dir = temp_dir();
    let mut session = McpSession::spawn(bin(), dir.path(), &["mcp"]);
    session.send(&json!({
        "jsonrpc": "2.0", "id": 7, "method": "server/discover",
        "params": {"_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        }}
    }));
    let discover = session.response(7, TIMEOUT);
    let versions = discover["result"]["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("server/discover: {discover}"));
    assert!(versions.iter().any(|v| v == "2026-07-28"), "{discover}");
    assert!(versions.iter().any(|v| v == "2025-06-18"), "{discover}");
    let (status, stderr) = session.finish(TIMEOUT);
    assert_eq!(status, 0, "{stderr}");
}

#[test]
fn immediate_eof_is_a_clean_shutdown() {
    let dir = temp_dir();
    let r = common::run_program(bin(), dir.path(), &["mcp"], Some(""));
    r.expect_status(0);
    assert!(r.stdout.is_empty(), "{}", r.stdout);
}

/// A timeline whose provider runs a local program.
const COMMAND_TIMELINE: &str = "version: \"1.0\"\nproviders:\n  local:\n    type: command\n    command: [\"/bin/sh\", \"-c\", \"exit 1\", \"sh\", \"{output}\"]\ntracks:\n  - id: v\n    type: speech\n    provider: local\n    voice_id: x\n    sequence:\n      - text: \"Hello\"\n";

/// The (code, path) of the error diagnostics `soniccraft_validate_timeline` reports for
/// [`COMMAND_TIMELINE`], through the server `program args…` started with `env`.
fn validate_errors(program: &str, args: &[&str], env: &[(&str, &str)]) -> Vec<(String, String)> {
    let dir = temp_dir();
    let mut session = McpSession::spawn_with_env(program, dir.path(), args, env);
    session.send(&json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "soniccraft-cli-test", "version": "0"}
        }
    }));
    session.response(0, TIMEOUT);
    session.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    session.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "soniccraft_validate_timeline",
            "arguments": {"yaml_content": COMMAND_TIMELINE}
        }
    }));
    let response = session.response(1, TIMEOUT);
    let report = &response["result"]["structuredContent"];
    let errors = report["diagnostics"]
        .as_array()
        .unwrap_or_else(|| panic!("no report: {response}"))
        .iter()
        .filter(|d| d["severity"] == "error")
        .map(|d| {
            (
                d["code"].as_str().unwrap_or_default().to_string(),
                d["path"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let (status, stderr) = session.finish(TIMEOUT);
    assert_eq!(status, 0, "{stderr}");
    errors
}

#[cfg(unix)] // the provider is a /bin/sh command
#[test]
fn command_providers_need_allow_command() {
    // The refusal is a `provider_unavailable` error at the provider (or its `command`).
    let refused = |errors: &[(String, String)]| {
        errors.iter().any(|(code, path)| {
            code == "provider_unavailable" && path.starts_with("providers.local")
        })
    };
    for (program, args, env) in [
        (bin(), &["mcp"][..], &[][..]),
        (bin(), &["mcp"], &[("SONICCRAFT_ALLOW_COMMAND", "0")]),
        (mcp_bin(), &[], &[]),
        (mcp_bin(), &[], &[("SONICCRAFT_ALLOW_COMMAND", "maybe")]),
    ] {
        let errors = validate_errors(program, args, env);
        assert!(refused(&errors), "{program} {args:?} {env:?}: {errors:?}");
    }
    for (program, args, env) in [
        (bin(), &["mcp", "--allow-command"][..], &[][..]),
        (bin(), &["mcp"], &[("SONICCRAFT_ALLOW_COMMAND", "1")]),
        (mcp_bin(), &[], &[("SONICCRAFT_ALLOW_COMMAND", "true")]),
    ] {
        let errors = validate_errors(program, args, env);
        assert!(!refused(&errors), "{program} {args:?} {env:?}: {errors:?}");
    }
    // The command line itself runs the user's own timelines: `command` providers work there.
    let dir = temp_dir();
    std::fs::write(dir.path().join("t.yaml"), COMMAND_TIMELINE).expect("write");
    let r = common::run(dir.path(), &["validate", "t.yaml", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).expect("report");
    assert!(
        !report["diagnostics"]
            .as_array()
            .is_some_and(|d| d.iter().any(|d| d["code"] == "provider_unavailable")),
        "{report}"
    );
}
