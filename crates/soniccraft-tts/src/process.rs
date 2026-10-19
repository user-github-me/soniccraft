//! Child-process helpers for the piper, macos_say and command providers.
//!
//! Children inherit SonicCraft's environment minus the provider API keys ([`child_env_removals`]), run in a
//! process group of their own on unix (a timeout or a cancelled render kills the whole group, grandchildren
//! included), and are finished when the child itself exits: output still buffered in the pipes is drained
//! for a short grace period, so a backgrounded grandchild holding stdout open cannot turn a successful run
//! into a timeout. Captured stdout is capped; stderr keeps its tail.
//!
//! A group of its own is outside the terminal's foreground group, so a Ctrl-C no longer reaches the child
//! directly: a render cancelled by dropping its future kills the group, and an `atexit` handler kills every
//! group still running when the process ends through `exit` (e.g. `std::process::exit` on a second Ctrl-C),
//! which skips destructors.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use soniccraft_core::ir::ProviderInstance;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::env::EnvSource;

/// Lines of stderr kept in error messages.
const STDERR_TAIL_LINES: usize = 20;
/// Characters of stderr kept in error messages.
const STDERR_TAIL_CHARS: usize = 2_000;
/// Bytes of stderr kept while a child runs (its end).
const STDERR_KEEP_BYTES: usize = 64 * 1024;
/// How long output is still read after the child exited (grandchildren may hold the pipes open).
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Provider API-key variables never passed to child processes (plus every `api_key_env` the caller names).
const SENSITIVE_ENV: [&str; 5] = [
    "ELEVENLABS_API_KEY",
    "OPENAI_API_KEY",
    "CARTESIA_API_KEY",
    "KOKORO_API_KEY",
    crate::keys::ALLOWED_HOSTS_ENV,
];

/// Variables removed from a child's environment: the built-in provider keys, `SONICCRAFT_ALLOWED_HOSTS`,
/// the instance's own `api_key_env` and `extra` (e.g. every `api_key_env` of the timeline's providers).
pub(crate) fn child_env_removals(origin: &ProviderInstance, extra: &[String]) -> Vec<String> {
    let mut vars: Vec<String> = SENSITIVE_ENV.iter().map(|v| (*v).to_owned()).collect();
    for var in origin.api_key_env.iter().chain(extra) {
        let var = var.trim();
        if !var.is_empty() && !vars.iter().any(|v| v == var) {
            vars.push(var.to_owned());
        }
    }
    vars
}

/// Captured result of a finished child process.
#[derive(Debug)]
pub(crate) struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a child process could not be run to completion.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    /// The program could not be started (not found, not executable).
    #[error("could not start `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    /// The program ran longer than allowed and was killed.
    #[error(
        "`{program}` did not finish within {timeout_ms} ms and was killed (with its child processes)"
    )]
    Timeout { program: String, timeout_ms: u64 },
    /// The program wrote more to stdout than is kept.
    #[error("`{program}` wrote more than {} MiB to stdout and was killed", limit >> 20)]
    OutputTooLarge { program: String, limit: usize },
    /// Waiting for the program or writing its stdin failed.
    #[error("I/O error while running `{program}`: {source}")]
    Io {
        program: String,
        #[source]
        source: io::Error,
    },
}

/// How to run a child.
pub(crate) struct Spawn<'a> {
    pub program: &'a Path,
    pub args: &'a [OsString],
    /// Working directory (inherited when `None`).
    pub cwd: Option<&'a Path>,
    /// Written to the child's stdin; otherwise stdin is `/dev/null`, never a TTY.
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    /// Variables removed from the inherited environment.
    pub env_remove: &'a [String],
    /// Most stdout bytes kept; a child writing more is killed ([`RunError::OutputTooLarge`]).
    pub stdout_limit: usize,
}

/// Process groups of children that are still running, killed by [`kill_live_groups`] at process exit.
static LIVE_GROUPS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

/// `atexit` handler: SIGKILL every child process group still running. Never blocks (a registry that is
/// locked at that instant is skipped) and never panics (it runs inside `exit`).
#[cfg(unix)]
extern "C" fn kill_live_groups() {
    let groups = match LIVE_GROUPS.try_lock() {
        Ok(groups) => groups,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    for &pgid in groups.iter() {
        // SAFETY: kill(2) has no memory-safety preconditions. A group is unregistered right after its
        // leader is reaped, so a registered id names a group of ours (but for that instant).
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Record a running child's group, installing the `atexit` handler on first use.
fn register_group(pgid: i32) {
    #[cfg(unix)]
    {
        static AT_EXIT: std::sync::Once = std::sync::Once::new();
        AT_EXIT.call_once(|| {
            // SAFETY: `kill_live_groups` is a plain `extern "C"` function that neither unwinds nor touches
            // state that `exit` tears down before running handlers.
            unsafe {
                libc::atexit(kill_live_groups);
            }
        });
    }
    LIVE_GROUPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(pgid);
}

fn unregister_group(pgid: i32) {
    LIVE_GROUPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&pgid);
}

/// Kills the child's process group (unix) when dropped, unless disarmed: a timed-out or cancelled child
/// takes its grandchildren with it. While armed, the group is also registered for [`kill_live_groups`].
struct GroupGuard {
    pgid: Option<i32>,
}

impl GroupGuard {
    fn new(child: &tokio::process::Child) -> Self {
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());
        if let Some(pgid) = pgid {
            register_group(pgid);
        }
        GroupGuard { pgid }
    }

    /// Kill the group now.
    fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            #[cfg(unix)]
            // SAFETY: kill(2) has no memory-safety preconditions; a negative pid addresses the process group
            // the child leads (it was spawned with `process_group(0)` and has not been reaped yet).
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            unregister_group(pgid);
        }
    }

    /// The child exited and was reaped: never signal the group afterwards (its id could be reused).
    fn disarm(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            unregister_group(pgid);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Output collected from one pipe by a background task.
struct Capture {
    data: Arc<Mutex<Vec<u8>>>,
    overflow: Arc<tokio::sync::Notify>,
    overflowed: Arc<std::sync::atomic::AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum Keep {
    /// The first bytes, up to the limit; more is an overflow.
    Head(usize),
    /// The last bytes.
    Tail(usize),
}

impl Capture {
    fn start<R>(pipe: Option<R>, keep: Keep) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let data = Arc::new(Mutex::new(Vec::new()));
        let overflow = Arc::new(tokio::sync::Notify::new());
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = pipe.map(|mut pipe| {
            let (data, overflow, overflowed) = (data.clone(), overflow.clone(), overflowed.clone());
            tokio::spawn(async move {
                let mut chunk = vec![0u8; 64 * 1024];
                loop {
                    let n = match pipe.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    let mut buf = data.lock().unwrap_or_else(PoisonError::into_inner);
                    match keep {
                        Keep::Head(limit) => {
                            if buf.len() + n > limit {
                                overflowed.store(true, std::sync::atomic::Ordering::SeqCst);
                                overflow.notify_one();
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        Keep::Tail(limit) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if buf.len() > 2 * limit {
                                let excess = buf.len() - limit;
                                buf.drain(..excess);
                            }
                        }
                    }
                }
            })
        });
        Capture {
            data,
            overflow,
            overflowed,
            task,
        }
    }

    /// Resolves when the head limit was passed.
    async fn overflowed(&self) {
        if self.task.is_none() {
            std::future::pending::<()>().await;
        }
        self.overflow.notified().await;
    }

    fn has_overflowed(&self) -> bool {
        self.overflowed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait up to `grace` for end of file, then take what was read.
    async fn finish(mut self, grace: Duration, keep: Keep) -> Vec<u8> {
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(grace, task).await.is_err() {
                abort.abort();
            }
        }
        let mut data =
            std::mem::take(&mut *self.data.lock().unwrap_or_else(PoisonError::into_inner));
        if let Keep::Tail(limit) = keep
            && data.len() > limit
        {
            data.drain(..data.len() - limit);
        }
        data
    }

    fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Run a child to completion (see the module docs). The child (and on unix its process group) is killed
/// when the timeout expires, when stdout passes its limit, or when the returned future is dropped.
pub(crate) async fn run(spawn: Spawn<'_>) -> Result<Output, RunError> {
    let name = spawn.program.display().to_string();
    let mut command = tokio::process::Command::new(spawn.program);
    command
        .args(spawn.args)
        .stdin(if spawn.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for var in spawn.env_remove {
        command.env_remove(var);
    }
    #[cfg(unix)]
    command.process_group(0);
    if let Some(dir) = spawn.cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn().map_err(|source| RunError::Spawn {
        program: name.clone(),
        source,
    })?;
    let mut group = GroupGuard::new(&child);
    let head = Keep::Head(spawn.stdout_limit);
    let tail = Keep::Tail(STDERR_KEEP_BYTES);
    let mut stdout = Capture::start(child.stdout.take(), head);
    let mut stderr = Capture::start(child.stderr.take(), tail);

    // Feed stdin from its own task while the captures drain stdout/stderr: writing everything first could
    // deadlock on a full pipe.
    let mut feeder = match (spawn.stdin, child.stdin.take()) {
        (Some(data), Some(mut pipe)) => Some(tokio::spawn(async move {
            let result = pipe.write_all(&data).await;
            drop(pipe); // EOF for the child
            result
        })),
        _ => None,
    };
    let stop_feeder = |feeder: &mut Option<tokio::task::JoinHandle<io::Result<()>>>| {
        if let Some(f) = feeder.take() {
            f.abort();
        }
    };

    enum Waited {
        Exited(io::Result<ExitStatus>),
        TimedOut,
        Overflow,
    }
    let waited = tokio::select! {
        status = child.wait() => Waited::Exited(status),
        () = tokio::time::sleep(spawn.timeout) => Waited::TimedOut,
        () = stdout.overflowed() => Waited::Overflow,
    };
    let status = match waited {
        Waited::Exited(Ok(status)) => {
            group.disarm();
            status
        }
        failure => {
            group.kill();
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            stop_feeder(&mut feeder);
            stdout.abort();
            stderr.abort();
            return Err(match failure {
                Waited::Exited(Err(source)) => RunError::Io {
                    program: name,
                    source,
                },
                Waited::Overflow => RunError::OutputTooLarge {
                    program: name,
                    limit: spawn.stdout_limit,
                },
                _ => RunError::Timeout {
                    program: name,
                    timeout_ms: spawn.timeout.as_millis() as u64,
                },
            });
        }
    };
    if stdout.has_overflowed() {
        return Err(RunError::OutputTooLarge {
            program: name,
            limit: spawn.stdout_limit,
        });
    }
    let stdout = stdout.finish(DRAIN_GRACE, head).await;
    let stderr = stderr.finish(DRAIN_GRACE, tail).await;

    if let Some(feeder) = feeder.take() {
        let abort = feeder.abort_handle();
        match tokio::time::timeout(DRAIN_GRACE, feeder).await {
            Ok(Ok(Ok(()))) => {}
            // A child that exits without reading all of its stdin is not an error by itself; a failing child
            // reports through its exit status.
            Ok(Ok(Err(e))) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Ok(Ok(Err(source))) if status.success() => {
                return Err(RunError::Io {
                    program: name,
                    source,
                });
            }
            Ok(Ok(Err(_))) => {}
            Ok(Err(join)) => {
                return Err(RunError::Io {
                    program: name,
                    source: io::Error::other(join.to_string()),
                });
            }
            // A grandchild holds stdin open without reading it: the child is done, so is its input.
            Err(_) => abort.abort(),
        }
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// The last lines of a child's stderr, for error messages.
pub(crate) fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..].join("\n");
    let count = tail.chars().count();
    if count > STDERR_TAIL_CHARS {
        let skip = count - STDERR_TAIL_CHARS;
        format!("…{}", tail.chars().skip(skip).collect::<String>())
    } else {
        tail
    }
}

/// Describe an exit status: `exit status 3` or `killed by signal 9`.
pub(crate) fn describe_status(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    status.to_string()
}

/// Resolve a program for spawning: names containing a path separator are paths (relative ones resolve
/// against `cwd`); bare names are looked up on `PATH` from `env`.
pub(crate) fn find_program(program: &str, env: &EnvSource, cwd: &Path) -> Option<PathBuf> {
    let has_separator = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if has_separator {
        let path = Path::new(program);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return path.is_file().then_some(path);
    }
    let search_path = env.get("PATH")?;
    which::which_in(program, Some(search_path), cwd).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn<'a>(program: &'a Path, args: &'a [OsString], timeout: Duration) -> Spawn<'a> {
        Spawn {
            program,
            args,
            cwd: None,
            stdin: None,
            timeout,
            env_remove: &[],
            stdout_limit: 1 << 20,
        }
    }

    #[test]
    fn stderr_tail_keeps_the_end() {
        let many: String = (0..50).map(|i| format!("line {i}\n")).collect();
        let tail = stderr_tail(many.as_bytes());
        assert!(tail.starts_with("line 30"), "{tail}");
        assert!(tail.ends_with("line 49"));
        let long = "x".repeat(5_000);
        let t = stderr_tail(long.as_bytes());
        assert_eq!(t.chars().count(), STDERR_TAIL_CHARS + 1);
        assert!(t.starts_with('…'));
        assert_eq!(stderr_tail(b"\n\n"), "");
    }

    #[test]
    fn env_removals_cover_keys_and_named_variables() {
        let mut o = crate::test_util::instance(soniccraft_core::ir::ProviderType::Command);
        o.api_key_env = Some("MY_TTS_KEY".into());
        let vars = child_env_removals(
            &o,
            &["GITHUB_TOKEN".into(), "OPENAI_API_KEY".into(), " ".into()],
        );
        for v in [
            "ELEVENLABS_API_KEY",
            "OPENAI_API_KEY",
            "CARTESIA_API_KEY",
            "KOKORO_API_KEY",
            "SONICCRAFT_ALLOWED_HOSTS",
            "MY_TTS_KEY",
            "GITHUB_TOKEN",
        ] {
            assert_eq!(vars.iter().filter(|x| *x == v).count(), 1, "{v}: {vars:?}");
        }
        assert_eq!(vars.len(), 7);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_with_stdin_and_captures_output() {
        let sh = find_program("sh", &EnvSource::Process, Path::new("/")).expect("sh on PATH");
        let args: Vec<OsString> = vec!["-c".into(), "cat; echo err >&2; exit 3".into()];
        let out = run(Spawn {
            stdin: Some(b"hello".to_vec()),
            ..spawn(&sh, &args, Duration::from_secs(10))
        })
        .await
        .unwrap();
        assert_eq!(out.stdout, b"hello");
        assert_eq!(stderr_tail(&out.stderr), "err");
        assert_eq!(describe_status(&out.status), "exit status 3");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_and_kills() {
        let sh = find_program("sh", &EnvSource::Process, Path::new("/")).expect("sh on PATH");
        let args: Vec<OsString> = vec!["-c".into(), "sleep 30".into()];
        let started = std::time::Instant::now();
        let err = run(spawn(&sh, &args, Duration::from_millis(200)))
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout { .. }), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_over_the_limit_kills_the_child() {
        let sh = find_program("sh", &EnvSource::Process, Path::new("/")).expect("sh on PATH");
        let args: Vec<OsString> = vec!["-c".into(), "yes soniccraft".into()];
        let started = std::time::Instant::now();
        let err = run(Spawn {
            stdout_limit: 100_000,
            ..spawn(&sh, &args, Duration::from_secs(20))
        })
        .await
        .unwrap_err();
        assert!(matches!(err, RunError::OutputTooLarge { .. }), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn removes_variables_from_the_child_environment() {
        let sh = find_program("sh", &EnvSource::Process, Path::new("/")).expect("sh on PATH");
        // HOME is set in every test environment; removing it proves the mechanism.
        let args: Vec<OsString> = vec!["-c".into(), "printf '%s' \"${HOME:-unset}\"".into()];
        let removed = ["HOME".to_owned()];
        let out = run(Spawn {
            env_remove: &removed,
            ..spawn(&sh, &args, Duration::from_secs(10))
        })
        .await
        .unwrap();
        assert_eq!(out.stdout, b"unset");
    }

    #[test]
    fn program_lookup() {
        let env = EnvSource::fixed([("PATH", "/nonexistent-dir")]);
        assert_eq!(find_program("sh", &env, Path::new("/")), None);
        assert_eq!(
            find_program("sh", &EnvSource::fixed([("X", "1")]), Path::new("/")),
            None
        );
        assert!(find_program("./definitely-missing.sh", &env, Path::new("/tmp")).is_none());
    }
}
