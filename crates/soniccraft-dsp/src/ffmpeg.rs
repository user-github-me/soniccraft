//! Locating and running the optional `ffmpeg` binary.
//!
//! ffmpeg is never required for the core formats (wav, mp3, flac). It is used as an encoder backend for
//! ogg/opus/m4a and as a decode fallback for anything symphonia cannot read (e.g. Opus).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How ffmpeg is located.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Ffmpeg {
    /// Look up `ffmpeg` on `PATH` when it is needed (the default).
    #[default]
    Auto,
    /// Use this binary.
    Path(PathBuf),
    /// Never use ffmpeg.
    Disabled,
}

impl Ffmpeg {
    /// Build a policy from an optional explicit binary path (`None` = [`Ffmpeg::Auto`]).
    pub fn from_option(path: Option<PathBuf>) -> Self {
        path.map_or(Ffmpeg::Auto, Ffmpeg::Path)
    }

    /// The binary to run, if any: the explicit path, the first `ffmpeg` on `PATH`, or `None`.
    pub fn resolve(&self) -> Option<PathBuf> {
        match self {
            Ffmpeg::Auto => find_ffmpeg(),
            Ffmpeg::Path(p) => Some(p.clone()),
            Ffmpeg::Disabled => None,
        }
    }
}

/// The first `ffmpeg` executable on `PATH`, if any.
pub fn find_ffmpeg() -> Option<PathBuf> {
    which::which("ffmpeg").ok()
}

/// Callback that writes ffmpeg's standard input. Runs on a separate thread so that ffmpeg can never
/// block on a full stdout/stderr pipe while we block on its stdin.
pub(crate) type StdinFeed<'a> = &'a (dyn Fn(&mut dyn Write) -> std::io::Result<()> + Sync);

/// Keep at most this many trailing bytes of ffmpeg's stderr in error messages.
const STDERR_TAIL: usize = 2000;

/// Run `binary` with `args` (`-hide_banner -loglevel error` are prepended), optionally feeding stdin,
/// and return its stdout. Errors carry the exit status and the tail of stderr.
pub(crate) fn run(
    binary: &Path,
    args: &[OsString],
    feed: Option<StdinFeed<'_>>,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(binary);
    command.args(["-hide_banner", "-loglevel", "error"]);
    if feed.is_none() {
        command.arg("-nostdin");
    }
    command
        .args(args)
        .stdin(if feed.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", binary.display()))?;
    let stdin = child.stdin.take();

    std::thread::scope(|scope| {
        let writer = match (stdin, feed) {
            (Some(mut pipe), Some(feed)) => Some(scope.spawn(move || {
                let result = feed(&mut pipe);
                drop(pipe); // close the pipe => EOF for ffmpeg
                result
            })),
            _ => None,
        };
        let output = child
            .wait_with_output()
            .map_err(|e| format!("waiting for ffmpeg failed: {e}"))?;
        let fed = match writer {
            Some(handle) => handle
                .join()
                .map_err(|_| "ffmpeg stdin writer panicked".to_string())?,
            None => Ok(()),
        };
        if !output.status.success() {
            // A BrokenPipe on stdin is only a symptom of ffmpeg exiting early; report its stderr.
            return Err(failure(output.status, &output.stderr));
        }
        fed.map_err(|e| format!("writing ffmpeg stdin failed: {e}"))?;
        Ok(output.stdout)
    })
}

/// The error message for a failed ffmpeg run: its exit status and the tail of its stderr.
fn failure(status: std::process::ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    let tail = if stderr.len() > STDERR_TAIL {
        let mut start = stderr.len() - STDERR_TAIL;
        while !stderr.is_char_boundary(start) {
            start += 1;
        }
        &stderr[start..]
    } else {
        stderr
    };
    format!(
        "ffmpeg failed ({status}): {}",
        if tail.is_empty() {
            "no error output"
        } else {
            tail
        }
    )
}

/// Bytes read from ffmpeg's stdout at a time by [`run_streaming`].
const PIPE_CHUNK: usize = 1 << 16;

/// Run `binary` with `args` (`-hide_banner -loglevel error -nostdin` are prepended) and hand its stdout
/// to `consume` chunk by chunk as it arrives, instead of collecting it.
///
/// The outer error is `consume`'s own (ffmpeg is then killed); the inner one says that ffmpeg could not
/// be started or failed, with its exit status and the tail of its stderr.
pub(crate) fn run_streaming<E>(
    binary: &Path,
    args: &[OsString],
    consume: &mut dyn FnMut(&[u8]) -> Result<(), E>,
) -> Result<Result<(), String>, E> {
    use std::io::Read;
    let mut child = match Command::new(binary)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Ok(Err(format!("could not start {}: {e}", binary.display()))),
    };
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(Err("ffmpeg's output pipes are missing".into()));
    };
    std::thread::scope(|scope| {
        // Drain stderr concurrently so that ffmpeg never blocks on a full pipe.
        let errors = scope.spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        let mut chunk = vec![0u8; PIPE_CHUNK];
        let mut stopped = None;
        let mut read_error = None;
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = consume(&chunk[..n]) {
                        stopped = Some(e);
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    read_error = Some(format!("reading ffmpeg's output failed: {e}"));
                    break;
                }
            }
        }
        if stopped.is_some() || read_error.is_some() {
            let _ = child.kill();
        }
        drop(stdout);
        let status = child.wait();
        let stderr = errors.join().unwrap_or_default();
        if let Some(e) = stopped {
            return Err(e);
        }
        Ok(match (status, read_error) {
            (_, Some(message)) => Err(message),
            (Err(e), None) => Err(format!("waiting for ffmpeg failed: {e}")),
            (Ok(status), None) if !status.success() => Err(failure(status, &stderr)),
            (Ok(_), None) => Ok(()),
        })
    })
}

/// A local path as an ffmpeg URL: the `file:` protocol prefix stops ffmpeg from reading a leading `-`
/// as an option or a `name:` prefix as a protocol.
pub(crate) fn file_url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path.as_os_str());
    url
}

/// Convenience: `OsString` arguments from string slices and paths.
pub(crate) fn args<I, S>(items: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    items.into_iter().map(Into::into).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_resolution() {
        assert_eq!(Ffmpeg::Disabled.resolve(), None);
        assert_eq!(
            Ffmpeg::Path(PathBuf::from("/x/ffmpeg")).resolve(),
            Some(PathBuf::from("/x/ffmpeg"))
        );
        assert_eq!(Ffmpeg::from_option(None), Ffmpeg::Auto);
    }

    #[test]
    fn missing_binary_is_an_error() {
        let err = run(
            Path::new("/definitely/not/ffmpeg"),
            &args(["-version"]),
            None,
        )
        .unwrap_err();
        assert!(err.contains("could not start"), "{err}");
    }

    #[test]
    fn runs_ffmpeg_when_available() {
        let Some(bin) = find_ffmpeg() else { return };
        let out = run(
            &bin,
            &args([
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono",
                "-t",
                "0.01",
                "-f",
                "s16le",
                "pipe:1",
            ]),
            None,
        )
        .unwrap();
        assert_eq!(out.len(), 160, "80 frames of s16 mono");
        let err = run(
            &bin,
            &args(["-f", "wav", "-i", "pipe:0", "-f", "null", "-"]),
            Some(&|w: &mut dyn Write| w.write_all(&[0u8; 4096])),
        )
        .unwrap_err();
        assert!(err.contains("ffmpeg failed"), "{err}");
    }

    #[test]
    fn streams_ffmpeg_output() {
        let missing =
            run_streaming::<()>(Path::new("/definitely/not/ffmpeg"), &[], &mut |_| Ok(()));
        assert!(missing.unwrap().unwrap_err().contains("could not start"));
        let Some(bin) = find_ffmpeg() else { return };
        // 10 s of 8 kHz s16 mono, far more than one pipe chunk.
        let tone = args([
            "-f",
            "lavfi",
            "-i",
            "sine=f=440:r=8000:d=10",
            "-f",
            "s16le",
            "pipe:1",
        ]);
        let mut total = 0usize;
        let mut chunks = 0usize;
        run_streaming::<()>(&bin, &tone, &mut |c| {
            total += c.len();
            chunks += 1;
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(total, 160_000);
        assert!(chunks > 1);
        // The consumer can stop it.
        let stopped = run_streaming(&bin, &tone, &mut |_| Err("enough"));
        assert_eq!(stopped.unwrap_err(), "enough");
        // ffmpeg's own failure carries its status.
        let bad = args([
            "-f",
            "lavfi",
            "-i",
            "nonsense_source",
            "-f",
            "s16le",
            "pipe:1",
        ]);
        let failed = run_streaming::<()>(&bin, &bad, &mut |_| Ok(())).unwrap();
        assert!(failed.unwrap_err().contains("ffmpeg failed"));
    }
}
