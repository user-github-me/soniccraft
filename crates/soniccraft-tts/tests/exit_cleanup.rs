//! A process that ends through `exit` (as `soniccraft render` does on a second Ctrl-C) skips destructors, and
//! children run in a process group of their own never see the terminal's Ctrl-C: the `atexit` handler must
//! kill them. The test re-runs itself as a child process that starts a long command and exits mid-render.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, json};
use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};
use soniccraft_tts::{SynthesisRequest, TtsClient, TtsOptions};

/// Set (to the scratch directory) in the re-run child process.
const CHILD_MODE: &str = "SONICCRAFT_TTS_EXIT_TEST_DIR";
const TEST_NAME: &str = "children_are_killed_when_the_process_exits_mid_render";

fn command_request(dir: &Path) -> SynthesisRequest {
    let provider = ProviderInstance {
        name: "command".into(),
        provider_type: ProviderType::Command,
        configured: true,
        api_key_env: None,
        base_url: None,
        model: None,
        output_format: None,
        max_concurrency: 1,
        timeout_ms: 60_000,
        chars_per_second: None,
        words_per_minute: None,
        settings: Map::new(),
        command: Some(vec!["./slow.sh".into(), "{output}".into()]),
        stdin: None,
        voices_dir: None,
    };
    let voice = VoiceIr {
        provider: "command".into(),
        provider_type: ProviderType::Command,
        voice_id: Some("v".into()),
        model: None,
        language: None,
        settings: json!({}).as_object().cloned().unwrap_or_default(),
    };
    SynthesisRequest {
        cue_id: "c1".into(),
        provider,
        voice,
        text: "Hello".into(),
        base_dir: dir.to_path_buf(),
    }
}

/// Child mode: start the render, wait until the grandchild runs, then leave through `exit`.
fn exit_mid_render(dir: PathBuf) -> ! {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async move {
        let client = TtsClient::new(TtsOptions::default());
        let request = command_request(&dir);
        let render = tokio::spawn(async move { client.synthesize(&request).await });
        let started = Instant::now();
        while !dir.join("grandchild.pid").exists() && started.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        std::mem::forget(render);
        std::process::exit(0)
    })
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn children_are_killed_when_the_process_exits_mid_render() {
    if let Some(dir) = std::env::var_os(CHILD_MODE) {
        exit_mid_render(PathBuf::from(dir));
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("slow.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nsleep 47 &\necho $! > \"$(dirname \"$0\")/grandchild.pid\"\nwait\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD_MODE, dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "child mode failed: {status}");
    let pid: i32 = std::fs::read_to_string(dir.path().join("grandchild.pid"))
        .expect("the command started")
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survived = alive(pid);
    if survived {
        // SAFETY: kill(2) of the test's own leftover `sleep`.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    assert!(!survived, "grandchild {pid} outlived the process");
}
