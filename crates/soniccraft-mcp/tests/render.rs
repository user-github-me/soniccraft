//! Long renders: progress notifications, concurrency, cancellation and disconnects.

// The slow-provider tests run a /bin/sh script, so they are Unix-only.
#![cfg_attr(not(unix), allow(unused_imports, dead_code))]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TestClient, mock_timeline, modern_meta, repo_root, text};
use serde_json::{Value, json};

/// A server that lets timelines run local programs (the slow provider below is a `command` provider).
fn command_client() -> TestClient {
    TestClient::start_configured(|options| options.with_allow_command(true))
}

/// A timeline whose `cues` cues each take about one second to synthesize, one at a time: a `command`
/// provider that logs its start to `log`, sleeps and copies a WAV to `{output}`.
#[cfg(unix)]
fn slow_timeline(dir: &Path, cues: usize) -> (String, PathBuf) {
    std::fs::create_dir_all(dir).expect("timeline directory");
    let tone = dir.join("tone.wav");
    std::fs::copy(
        repo_root().join("examples/offline-demo/assets/sfx/chime.wav"),
        &tone,
    )
    .expect("copy the test tone");
    let log = dir.join("synth.log");
    let mut yaml = format!(
        r#"version: "1.0"
providers:
  slow:
    type: command
    max_concurrency: 1
    command: ["/bin/sh", "-c", "echo started >> \"$2\"; sleep 1; cp \"$3\" \"$1\"", "sh", "{{output}}", {log:?}, {tone:?}]
tracks:
  - id: voice
    type: speech
    provider: slow
    voice_id: slow-voice
    sequence:
"#,
        log = log.display().to_string(),
        tone = tone.display().to_string(),
    );
    for i in 0..cues {
        yaml.push_str(&format!("      - text: \"Sentence number {i}.\"\n"));
    }
    (yaml, log)
}

#[cfg(unix)]
fn log_lines(log: &Path) -> usize {
    std::fs::read_to_string(log).map_or(0, |s| s.lines().count())
}

#[cfg(unix)]
async fn wait_for_log(log: &Path, lines: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while log_lines(log) < lines {
        assert!(Instant::now() < deadline, "the slow provider never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn progress_of(notifications: &[Value], token: &Value) -> Vec<f64> {
    notifications
        .iter()
        .filter(|n| &n["params"]["progressToken"] == token)
        .map(|n| n["params"]["progress"].as_f64().expect("numeric progress"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_notifications_increase_and_precede_the_response() {
    let mut client = TestClient::start();
    client.initialize().await;
    let token = json!(42);
    let response = client
        .request(
            json!(1),
            "tools/call",
            Some(json!({
                "name": "soniccraft_render",
                "arguments": {"yaml_content": mock_timeline("p.wav")},
                "_meta": {"progressToken": token}
            })),
        )
        .await;
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    let notifications = client.notifications("notifications/progress");
    let values = progress_of(&notifications, &token);
    assert!(values.len() >= 3, "{notifications:#?}");
    assert!(
        values.windows(2).all(|w| w[1] > w[0]),
        "strictly increasing: {values:?}"
    );
    for n in &notifications {
        assert_eq!(n["params"]["total"], 100.0);
        assert!(
            !n["params"]["message"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }
    // Every progress line was written before the response line.
    let response_line = client
        .raw_lines
        .iter()
        .position(|l| l.contains("\"id\":1,") && l.contains("\"result\""))
        .expect("response line");
    let last_progress = client
        .raw_lines
        .iter()
        .rposition(|l| l.contains("notifications/progress"))
        .expect("progress lines");
    assert!(last_progress < response_line);

    // Without a progress token there are no notifications.
    client.backlog.clear();
    let quiet = client
        .call_tool(
            json!(2),
            "soniccraft_render",
            json!({"yaml_content": mock_timeline("q.wav")}),
        )
        .await;
    assert_eq!(quiet["isError"], false);
    assert!(client.notifications("notifications/progress").is_empty());
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modern_render_with_progress() {
    let mut client = TestClient::start();
    let mut meta = modern_meta();
    meta["progressToken"] = json!("modern-1");
    let response = client
        .request(
            json!("r1"),
            "tools/call",
            Some(json!({
                "name": "soniccraft_render",
                "arguments": {"yaml_content": mock_timeline("m.wav")},
                "_meta": meta
            })),
        )
        .await;
    let result = &response["result"];
    assert_eq!(result["resultType"], "complete", "{response:#}");
    assert_eq!(result["isError"], false);
    assert!(
        !progress_of(
            &client.notifications("notifications/progress"),
            &json!("modern-1")
        )
        .is_empty()
    );
    client.shutdown().await.expect("clean shutdown");
}

#[cfg(unix)] // the slow provider is a /bin/sh script
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_requests_are_served_while_a_render_runs() {
    let mut client = command_client();
    client.initialize().await;
    let (yaml, log) = slow_timeline(client.base(), 3);
    client
        .send_request(
            json!("render"),
            "tools/call",
            Some(json!({"name": "soniccraft_render", "arguments": {"yaml_content": yaml, "output_path": "slow.wav"}})),
        )
        .await;
    wait_for_log(&log, 1).await;
    let started = Instant::now();
    let ping = client.request(json!("ping"), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    let validate = client
        .call_tool(
            json!("validate"),
            "soniccraft_validate_timeline",
            json!({"yaml_content": mock_timeline("v.wav")}),
        )
        .await;
    assert_eq!(validate["structuredContent"]["valid"], true);
    let list = client.request(json!("list"), "tools/list", None).await;
    assert!(list["result"]["tools"].is_array());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "answered while rendering: {:?}",
        started.elapsed()
    );
    assert!(log_lines(&log) < 3, "the render was still running");

    let render = client.response(json!("render")).await;
    assert_eq!(render["result"]["isError"], false, "{render:#}");
    assert_eq!(log_lines(&log), 3);
    assert!(client.base().join("slow.wav").exists());
    client.shutdown().await.expect("clean shutdown");
}

#[cfg(unix)] // the slow provider is a /bin/sh script
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_stops_the_render_and_sends_no_response() {
    let mut client = command_client();
    client.initialize().await;
    let (yaml, log) = slow_timeline(client.base(), 6);
    client
        .send_request(
            json!(7),
            "tools/call",
            Some(json!({
                "name": "soniccraft_render",
                "arguments": {"yaml_content": yaml, "output_path": "cancelled.wav"},
                "_meta": {"progressToken": "tok"}
            })),
        )
        .await;
    wait_for_log(&log, 1).await;
    client
        .notify(
            "notifications/cancelled",
            Some(json!({"requestId": 7, "reason": "test"})),
        )
        .await;
    let cancelled_at = Instant::now();
    let ping = client.request(json!(8), "ping", None).await;
    assert_eq!(ping["result"], json!({}), "the server stays responsive");

    // No response for the cancelled request, and no progress for it after a short settling time.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = client.notifications("notifications/progress").len();
    client
        .assert_silent_for(Duration::from_secs(3), |m| {
            m.get("id") == Some(&json!(7)) || m["method"] == "notifications/progress"
        })
        .await;
    assert_eq!(client.notifications("notifications/progress").len(), before);

    // The engine really stopped: far fewer than 6 synthesis requests ran, and nothing was written.
    let ran = log_lines(&log);
    assert!(
        ran <= 2,
        "{ran} synthesis requests ran after cancelling at {cancelled_at:?}"
    );
    assert!(!client.base().join("cancelled.wav").exists());

    let ping = client.request(json!(9), "ping", None).await;
    assert_eq!(ping["result"], json!({}));
    client.shutdown().await.expect("clean shutdown");
}

#[cfg(unix)] // the slow provider is a /bin/sh script
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnecting_mid_render_cancels_it_and_exits_cleanly() {
    let mut client = command_client();
    client.initialize().await;
    let (yaml, log) = slow_timeline(client.base(), 20);
    client
        .send_request(
            json!(1),
            "tools/call",
            Some(json!({"name": "soniccraft_render", "arguments": {"yaml_content": yaml, "output_path": "gone.wav"}})),
        )
        .await;
    wait_for_log(&log, 1).await;
    let base = client.base().to_path_buf();
    let dir = tempfile::tempdir().unwrap();
    // Keep the temporary directory alive past `shutdown` (it consumes the client).
    let started = Instant::now();
    let keep = std::mem::replace(&mut client.dir, dir);
    client.shutdown().await.expect("clean shutdown after EOF");
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "{:?}",
        started.elapsed()
    );
    let ran = log_lines(&log);
    assert!(
        ran < 20,
        "the abandoned render was cancelled ({ran} cues ran)"
    );
    assert!(!base.join("gone.wav").exists());
    drop(keep);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_does_not_lose_the_result_when_the_client_closes_its_input() {
    // rmcp stops forwarding notifications once input closes; progress must never hold the result back.
    let mut client = TestClient::start();
    client.initialize().await;
    client
        .send_request(
            json!(1),
            "tools/call",
            Some(json!({
                "name": "soniccraft_render",
                "arguments": {"yaml_content": mock_timeline("eof-progress.wav")},
                "_meta": {"progressToken": "eof"}
            })),
        )
        .await;
    client.close_input();
    let started = Instant::now();
    let response = client.response(json!(1)).await;
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    client.shutdown().await.expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quick_requests_complete_after_eof() {
    // A client that writes its requests and closes its input still gets every answer (rmcp drains
    // in-flight requests before closing).
    let mut client = TestClient::start();
    client.initialize().await;
    client
        .send_request(
            json!(1),
            "tools/call",
            Some(json!({"name": "soniccraft_render", "arguments": {"yaml_content": mock_timeline("eof.wav")}})),
        )
        .await;
    client.close_input();
    let response = client.response(json!(1)).await;
    assert_eq!(response["result"]["isError"], false, "{response:#}");
    assert!(text(&response["result"], 0).starts_with("Rendered "));
    client.shutdown().await.expect("clean shutdown");
}

#[cfg(unix)] // the provider is a /bin/sh script
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_providers_need_the_servers_opt_in() {
    // A command-provider timeline that leaves a marker file next to itself when its program runs.
    fn marker_timeline(dir: &Path) -> (String, PathBuf) {
        let marker = dir.join("ran.txt");
        let tone = dir.join("tone.wav");
        std::fs::copy(
            repo_root().join("examples/offline-demo/assets/sfx/chime.wav"),
            &tone,
        )
        .unwrap();
        let yaml = format!(
            r#"version: "1.0"
providers:
  local:
    type: command
    command: ["/bin/sh", "-c", "echo ran > \"$2\"; cp \"$3\" \"$1\"", "sh", "{{output}}", {marker:?}, {tone:?}]
tracks:
  - id: v
    type: speech
    provider: local
    voice_id: x
    sequence: [{{text: "Hello."}}]
"#,
            marker = marker.display().to_string(),
            tone = tone.display().to_string(),
        );
        (yaml, marker)
    }

    // A timeline is data: by default the server refuses to run the programs it names.
    let mut client = TestClient::start();
    client.initialize().await;
    let (yaml, marker) = marker_timeline(client.base());
    let validate = client
        .call_tool(
            json!(1),
            "soniccraft_validate_timeline",
            json!({"yaml_content": yaml}),
        )
        .await;
    assert_eq!(
        validate["structuredContent"]["valid"], false,
        "{validate:#}"
    );
    let render = client
        .call_tool(
            json!(2),
            "soniccraft_render",
            json!({"yaml_content": yaml, "output_path": "out.wav"}),
        )
        .await;
    assert_eq!(render["isError"], true, "{render:#}");
    let diagnostics = render["structuredContent"]["diagnostics"]
        .as_array()
        .unwrap();
    let refused = diagnostics
        .iter()
        .find(|d| d["code"] == "provider_unavailable")
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    assert!(
        refused["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("--allow-command"),
        "{refused:#}"
    );
    assert!(!marker.exists(), "the program never ran");
    client.shutdown().await.expect("clean shutdown");

    // With the opt-in it runs.
    let mut client = command_client();
    client.initialize().await;
    let (yaml, marker) = marker_timeline(client.base());
    let render = client
        .call_tool(
            json!(3),
            "soniccraft_render",
            json!({"yaml_content": yaml, "output_path": "out.wav"}),
        )
        .await;
    assert_eq!(render["isError"], false, "{render:#}");
    assert!(marker.exists());
    client.shutdown().await.expect("clean shutdown");
}

#[cfg(unix)] // the slow provider is a /bin/sh script
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renders_wait_for_a_free_slot_and_can_be_cancelled_while_waiting() {
    let mut client = TestClient::start_configured(|options| {
        options
            .with_allow_command(true)
            .with_max_concurrent_renders(1)
    });
    client.initialize().await;
    let (first, first_log) = slow_timeline(&client.base().join("first"), 2);
    let (second, second_log) = slow_timeline(&client.base().join("second"), 1);
    let (third, third_log) = slow_timeline(&client.base().join("third"), 1);
    client
        .send_request(
            json!("first"),
            "tools/call",
            Some(json!({"name": "soniccraft_render", "arguments": {"yaml_content": first, "output_path": "first.wav"}})),
        )
        .await;
    wait_for_log(&first_log, 1).await;
    client
        .send_request(
            json!("second"),
            "tools/call",
            Some(json!({
                "name": "soniccraft_render",
                "arguments": {"yaml_content": second, "output_path": "second.wav"},
                "_meta": {"progressToken": "queued"}
            })),
        )
        .await;
    client
        .send_request(
            json!("third"),
            "tools/call",
            Some(json!({"name": "soniccraft_render", "arguments": {"yaml_content": third, "output_path": "third.wav"}})),
        )
        .await;
    // The second render reports that it waits, before the first one is done.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiting = client
            .notifications("notifications/progress")
            .into_iter()
            .find(|n| n["params"]["progressToken"] == "queued");
        if let Some(n) = waiting {
            let message = n["params"]["message"].as_str().unwrap();
            assert!(
                message.contains("Waiting for another render to finish"),
                "{message}"
            );
            assert!(n["params"]["progress"].as_f64().unwrap() < 1.0);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no waiting notification: {:#?}",
            client.backlog
        );
        if let Some(message) = client.next_message(Duration::from_millis(200)).await {
            client.backlog.push(message);
        }
    }
    assert!(
        log_lines(&first_log) < 2,
        "the first render is still running"
    );
    assert_eq!(
        log_lines(&second_log),
        0,
        "the second render has not started"
    );
    // Cancel the third while it waits: it never starts and gets no response.
    client
        .notify(
            "notifications/cancelled",
            Some(json!({"requestId": "third", "reason": "test"})),
        )
        .await;

    let first_done = client.response(json!("first")).await;
    assert_eq!(first_done["result"]["isError"], false, "{first_done:#}");
    let second_done = client.response(json!("second")).await;
    assert_eq!(second_done["result"]["isError"], false, "{second_done:#}");
    assert_eq!(log_lines(&second_log), 1);
    client
        .assert_silent_for(Duration::from_secs(2), |m| {
            m.get("id") == Some(&json!("third"))
        })
        .await;
    assert_eq!(log_lines(&third_log), 0, "the cancelled render never ran");
    assert!(!client.base().join("third.wav").exists());
    // Progress values of the queued render still only increase.
    let values = progress_of(
        &client.notifications("notifications/progress"),
        &json!("queued"),
    );
    assert!(values.windows(2).all(|w| w[1] > w[0]), "{values:?}");
    client.shutdown().await.expect("clean shutdown");
}
