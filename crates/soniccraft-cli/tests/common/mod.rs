//! Helpers for the integration tests: run the built binaries in temporary directories with a clean,
//! deterministic environment.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

/// The `soniccraft` binary under test.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_soniccraft")
}

/// The `soniccraft-mcp` launcher under test.
pub fn mcp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_soniccraft-mcp")
}

/// The workspace root (the repository checkout).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Environment variables removed from every child so results do not depend on the developer's shell.
const SCRUBBED: [&str; 15] = [
    "RUST_LOG",
    "SONICCRAFT_CACHE_DIR",
    "SONICCRAFT_ALLOW_COMMAND",
    "SONICCRAFT_ALLOWED_HOSTS",
    "SONICCRAFT_MAX_RENDER_GB",
    "SONICCRAFT_MCP_MAX_RENDERS",
    "CLAUDE_PROJECT_DIR",
    "ELEVENLABS_API_KEY",
    "OPENAI_API_KEY",
    "CARTESIA_API_KEY",
    "KOKORO_API_KEY",
    "KOKORO_BASE_URL",
    "PIPER_VOICES_DIR",
    "CLICOLOR_FORCE",
    "RUST_BACKTRACE",
];

/// A command for `program` with a scrubbed environment, no colors and `cwd` as working directory.
pub fn command(program: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd).env("NO_COLOR", "1");
    for var in SCRUBBED {
        cmd.env_remove(var);
    }
    // An unreachable Kokoro server, so `provider_unavailable` is deterministic and fast.
    cmd.env("KOKORO_BASE_URL", "http://127.0.0.1:9");
    cmd
}

/// The outcome of one run.
#[derive(Debug)]
pub struct Run {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// stdout parsed as exactly one JSON document.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not one JSON document ({e}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }

    /// Panic with both streams unless the exit status is `expected`.
    pub fn expect_status(&self, expected: i32) -> &Self {
        assert_eq!(
            self.status, expected,
            "unexpected exit status\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }
}

/// Run `soniccraft args…` in `cwd`.
pub fn run(cwd: &Path, args: &[&str]) -> Run {
    run_program(bin(), cwd, args, None)
}

/// Run `soniccraft args…` in `cwd` with `stdin` as input.
pub fn run_with_stdin(cwd: &Path, args: &[&str], stdin: &str) -> Run {
    run_program(bin(), cwd, args, Some(stdin))
}

/// Run `program args…` in `cwd`.
pub fn run_program(program: &str, cwd: &Path, args: &[&str], stdin: Option<&str>) -> Run {
    let mut cmd = command(program, cwd);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn the binary");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin pipe");
        pipe.write_all(input.as_bytes()).expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait for the binary");
    Run {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A fresh temporary directory.
pub fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("soniccraft-cli-test")
        .tempdir()
        .expect("temp dir")
}

/// A line-oriented MCP client over a child's stdin/stdout, with timeouts so a broken server fails the
/// test instead of hanging it.
pub struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl McpSession {
    /// Spawn `program args…` as an MCP server.
    pub fn spawn(program: &str, cwd: &Path, args: &[&str]) -> Self {
        Self::spawn_with_env(program, cwd, args, &[])
    }

    /// Spawn `program args…` as an MCP server with extra environment variables.
    pub fn spawn_with_env(program: &str, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut child = command(program, cwd)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the MCP server");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout pipe");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        McpSession {
            child,
            stdin,
            lines,
        }
    }

    /// Send one JSON-RPC message (one line).
    pub fn send(&mut self, message: &serde_json::Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        let mut line = serde_json::to_string(message).expect("serialize");
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .expect("write to the server");
        stdin.flush().expect("flush");
    }

    /// The next message the server writes; every line must be one JSON object.
    pub fn recv(&mut self, timeout: Duration) -> serde_json::Value {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("server wrote a non-JSON line ({e}): {line}")),
            Err(RecvTimeoutError::Timeout) => {
                panic!("no message from the server within {timeout:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("the server closed stdout"),
        }
    }

    /// The response to request `id`, skipping notifications.
    pub fn response(&mut self, id: i64, timeout: Duration) -> serde_json::Value {
        loop {
            let message = self.recv(timeout);
            if message.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    /// Close stdin and wait for the exit status (killing the server after `timeout`).
    pub fn finish(mut self, timeout: Duration) -> (i32, String) {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    let mut stderr = String::new();
                    if let Some(mut err) = self.child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut err, &mut stderr);
                    }
                    return (status.code().unwrap_or(-1), stderr);
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = self.child.kill();
                    panic!("the MCP server did not exit within {timeout:?} after stdin closed");
                }
            }
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
