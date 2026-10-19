//! A scripted MCP client for in-process tests: the server runs [`soniccraft_mcp::serve`] on in-memory
//! duplex pipes, and the client writes raw JSON lines and reads the server's output line by line.

#![allow(dead_code)] // each test binary uses a different subset

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use soniccraft_engine::{Engine, EngineOptions};
use soniccraft_mcp::ServerOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio::task::JoinHandle;

/// Default wait for one message.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// The newest protocol revision that still uses `initialize`.
pub const LEGACY_LATEST: &str = "2025-11-25";

/// Per-request `_meta` of the stateless 2026-07-28 revision.
pub fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "soniccraft-tests", "version": "0"}
    })
}

/// Repository root (for example assets).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A running server plus a client connected to it.
pub struct TestClient {
    tx: Option<DuplexStream>,
    rx: Lines<BufReader<DuplexStream>>,
    server: Option<JoinHandle<std::io::Result<()>>>,
    /// Messages received while waiting for something else (notifications, other responses).
    pub backlog: Vec<Value>,
    /// Every line the server wrote, verbatim (to check framing).
    pub raw_lines: Vec<String>,
    /// Scratch directory: the default base directory and the TTS cache live here.
    pub dir: tempfile::TempDir,
}

impl TestClient {
    /// Start a server whose base directory and cache are a fresh temporary directory.
    pub fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().to_path_buf();
        Self::start_with(dir, ServerOptions::with_base_dir(base))
    }

    /// Start a server whose base directory is a fresh temporary directory, with `configure` applied to
    /// the default options (e.g. `|o| o.with_allow_command(true)`).
    pub fn start_configured(configure: impl FnOnce(ServerOptions) -> ServerOptions) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().to_path_buf();
        Self::start_with(dir, configure(ServerOptions::with_base_dir(base)))
    }

    /// Start a server with explicit options (the cache still lives in `dir`).
    pub fn start_with(dir: tempfile::TempDir, options: ServerOptions) -> Self {
        let engine = Engine::new(EngineOptions {
            cache_dir: Some(dir.path().join("tts-cache")),
            ..EngineOptions::default()
        });
        let (client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(soniccraft_mcp::serve(
            engine, options, server_in, server_out,
        ));
        TestClient {
            tx: Some(client_in),
            rx: BufReader::new(client_out).lines(),
            server: Some(server),
            backlog: Vec::new(),
            raw_lines: Vec::new(),
            dir,
        }
    }

    /// The base directory the server was started with.
    pub fn base(&self) -> &Path {
        self.dir.path()
    }

    /// Write one raw line (a newline is appended).
    pub async fn send_line(&mut self, line: &str) {
        let tx = self.tx.as_mut().expect("client input is open");
        tx.write_all(line.as_bytes()).await.expect("write");
        tx.write_all(b"\n").await.expect("write");
        tx.flush().await.expect("flush");
    }

    /// Write raw bytes (no newline added).
    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        let tx = self.tx.as_mut().expect("client input is open");
        tx.write_all(bytes).await.expect("write");
        tx.flush().await.expect("flush");
    }

    /// Write one JSON message.
    pub async fn send(&mut self, message: Value) {
        self.send_line(&message.to_string()).await;
    }

    /// Send a request.
    pub async fn send_request(&mut self, id: Value, method: &str, params: Option<Value>) {
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(message).await;
    }

    /// Send a notification.
    pub async fn notify(&mut self, method: &str, params: Option<Value>) {
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(message).await;
    }

    /// The next message from the server, or `None` on timeout / end of output.
    pub async fn next_message(&mut self, wait: Duration) -> Option<Value> {
        match tokio::time::timeout(wait, self.rx.next_line()).await {
            Ok(Ok(Some(line))) => {
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("server wrote a non-JSON line ({e}): {line:?}"));
                self.raw_lines.push(line);
                Some(value)
            }
            Ok(Ok(None)) | Err(_) => None,
            Ok(Err(e)) => panic!("reading server output failed: {e}"),
        }
    }

    /// Wait for the response (result or error) with `id`; other messages go to the backlog.
    pub async fn response(&mut self, id: Value) -> Value {
        self.response_within(id, TIMEOUT).await
    }

    /// Like [`Self::response`] with a custom timeout.
    pub async fn response_within(&mut self, id: Value, wait: Duration) -> Value {
        if let Some(pos) = self.backlog.iter().position(|m| is_response_to(m, &id)) {
            return self.backlog.remove(pos);
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(message) = self.next_message(left).await else {
                panic!(
                    "no response to id {id} within {wait:?}; backlog: {:#?}",
                    self.backlog
                );
            };
            if is_response_to(&message, &id) {
                return message;
            }
            self.backlog.push(message);
        }
    }

    /// Send a request and wait for its response.
    pub async fn request(&mut self, id: Value, method: &str, params: Option<Value>) -> Value {
        self.send_request(id.clone(), method, params).await;
        self.response(id).await
    }

    /// Legacy handshake with `version`; returns the initialize response.
    pub async fn initialize_with(&mut self, version: &str) -> Value {
        let response = self
            .request(
                json!(0),
                "initialize",
                Some(json!({
                    "protocolVersion": version,
                    "capabilities": {},
                    "clientInfo": {"name": "soniccraft-tests", "version": "0"}
                })),
            )
            .await;
        self.notify("notifications/initialized", None).await;
        response
    }

    /// Legacy handshake as Codex does it (2025-06-18).
    pub async fn initialize(&mut self) -> Value {
        let response = self.initialize_with("2025-06-18").await;
        assert_eq!(
            response["result"]["protocolVersion"], "2025-06-18",
            "{response}"
        );
        response
    }

    /// Call a tool and return the JSON-RPC response.
    pub async fn call_tool_response(&mut self, id: Value, tool: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            Some(json!({"name": tool, "arguments": arguments})),
        )
        .await
    }

    /// Call a tool and return its result object (panics on a JSON-RPC error).
    pub async fn call_tool(&mut self, id: Value, tool: &str, arguments: Value) -> Value {
        let response = self.call_tool_response(id, tool, arguments).await;
        assert!(
            response.get("error").is_none(),
            "{tool} returned a JSON-RPC error: {response:#}"
        );
        response["result"].clone()
    }

    /// Notifications with `method` received so far (from the backlog).
    pub fn notifications(&self, method: &str) -> Vec<Value> {
        self.backlog
            .iter()
            .filter(|m| m.get("id").is_none() && m["method"] == method)
            .cloned()
            .collect()
    }

    /// Assert that the server sends nothing for `wait` (messages that do arrive go to the backlog and
    /// fail the assertion only if `forbidden` matches them).
    pub async fn assert_silent_for(&mut self, wait: Duration, forbidden: impl Fn(&Value) -> bool) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return;
            }
            match self.next_message(left).await {
                Some(message) => {
                    assert!(!forbidden(&message), "unexpected message: {message:#}");
                    self.backlog.push(message);
                }
                None => return,
            }
        }
    }

    /// Close the client's input (end of file for the server).
    pub fn close_input(&mut self) {
        self.tx = None;
    }

    /// Close the input and wait for the server to exit; returns its result.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        self.close_input();
        // Drain remaining output so the server never blocks on a full pipe.
        let server = self.server.take().expect("server task");
        let drain = async {
            while let Ok(Some(line)) = self.rx.next_line().await {
                self.raw_lines.push(line);
            }
        };
        let ((), result) = tokio::join!(drain, async {
            tokio::time::timeout(Duration::from_secs(30), server)
                .await
                .expect("server exits after EOF")
                .expect("server task does not panic")
        });
        result
    }
}

fn is_response_to(message: &Value, id: &Value) -> bool {
    message.get("id") == Some(id)
        && (message.get("result").is_some() || message.get("error").is_some())
}

/// The text of content block `index` of a tool result.
pub fn text(result: &Value, index: usize) -> String {
    let block = &result["content"][index];
    assert_eq!(
        block["type"], "text",
        "content[{index}] is not text: {result:#}"
    );
    block["text"].as_str().unwrap_or_default().to_string()
}

/// A minimal valid timeline using the offline mock voice.
pub fn mock_timeline(output: &str) -> String {
    format!(
        r#"version: "1.0"
title: "MCP test"
output:
  path: {output}
  bit_depth: 16
speakers:
  narrator: {{ provider: mock, voice_id: narrator, label: Narrator }}
  guest: {{ provider: mock, voice_id: guest, label: Guest }}
tracks:
  - id: voice_a
    type: speech
    speaker: narrator
    sequence:
      - id: cue_01
        text: "Hello from SonicCraft."
      - pause: 300ms
      - id: cue_03
        text: "The render is mastered and subtitled."
  - id: voice_b
    type: speech
    speaker: guest
    sequence:
      - id: cue_02
        start_relative: {{ after_cue: cue_01, offset_ms: 150 }}
        text: "Over a model context protocol."
"#
    )
}

/// A long two-speaker mock timeline: `cues` cues separated by 200 ms pauses, 8 kHz mono so renders are
/// quick (the shape of the reviewers' `big200.yaml`).
pub fn long_mock_timeline(cues: usize, output: &str) -> String {
    let mut yaml = format!(
        r#"version: "1.0"
output: {{ path: {output}, bit_depth: 16, sample_rate: 8000, channels: 1 }}
speakers:
  a: {{ provider: mock, voice_id: a, label: A }}
  b: {{ provider: mock, voice_id: b, label: B }}
tracks:
  - id: talk
    type: speech
    sequence:
"#
    );
    for i in 0..cues {
        let speaker = if i.is_multiple_of(2) { "a" } else { "b" };
        yaml.push_str(&format!(
            "      - {{ id: c{i:03}, speaker: {speaker}, text: \"Line number {i} of a long conversation about nothing much.\" }}\n"
        ));
        if i + 1 < cues {
            yaml.push_str("      - pause: 200ms\n");
        }
    }
    yaml
}

/// Total size of a tool result as an agent receives it: every text block plus `structuredContent`.
pub fn result_chars(result: &Value) -> usize {
    let texts: usize = result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .map(|t| t.chars().count())
                .sum()
        })
        .unwrap_or(0);
    let structured = result
        .get("structuredContent")
        .map_or(0, |s| s.to_string().chars().count());
    texts + structured
}

/// The content block types of a tool result, in order.
pub fn content_types(result: &Value) -> Vec<String> {
    result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| b["type"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}
