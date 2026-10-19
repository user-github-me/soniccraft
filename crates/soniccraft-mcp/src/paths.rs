//! Path handling: the server's default base directory, resolution of relative tool arguments, and
//! `file://` URIs for resource links.
//!
//! Precedence of the directory that relative paths resolve against (DESIGN §9.2): the tool argument
//! `base_dir` > the server option (`soniccraft mcp --base-dir`) > env `CLAUDE_PROJECT_DIR` > the process's
//! working directory. Paths are made absolute and normalized lexically (`.` and `..` folded) but symlinks
//! are not resolved, so reports show the paths the caller wrote.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Environment variable Claude Code sets to the project root for the MCP servers it launches.
pub(crate) const PROJECT_DIR_ENV: &str = "CLAUDE_PROJECT_DIR";

/// Where the server's default base directory came from (for logs and the server instructions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseDirSource {
    /// `ServerOptions::base_dir` (`soniccraft mcp --base-dir`).
    Option,
    /// The `CLAUDE_PROJECT_DIR` environment variable.
    ProjectEnv,
    /// The process's current working directory.
    WorkingDir,
}

impl BaseDirSource {
    /// Short human description.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            BaseDirSource::Option => "--base-dir",
            BaseDirSource::ProjectEnv => "CLAUDE_PROJECT_DIR",
            BaseDirSource::WorkingDir => "the server's working directory",
        }
    }
}

/// Resolve the server's default base directory from the configured option, `CLAUDE_PROJECT_DIR` and the
/// working directory, in that order. `project_env` is the value of `CLAUDE_PROJECT_DIR` (passed in so the
/// logic is testable); an empty value or one that is not an existing directory is skipped with a warning.
pub(crate) fn default_base_dir(
    configured: Option<&Path>,
    project_env: Option<&OsStr>,
) -> (PathBuf, BaseDirSource) {
    let cwd = std::env::current_dir().unwrap_or_else(|error| {
        tracing::warn!(%error, "cannot read the working directory; using `.`");
        PathBuf::from(".")
    });
    if let Some(dir) = configured.filter(|d| !d.as_os_str().is_empty()) {
        let dir = absolutize(&cwd, &expand_home(dir));
        if !dir.is_dir() {
            tracing::warn!(dir = %dir.display(), "--base-dir is not an existing directory");
        }
        return (dir, BaseDirSource::Option);
    }
    if let Some(value) = project_env.filter(|v| !v.is_empty()) {
        let dir = absolutize(&cwd, Path::new(value));
        if dir.is_dir() {
            return (dir, BaseDirSource::ProjectEnv);
        }
        tracing::warn!(
            dir = %dir.display(),
            "{PROJECT_DIR_ENV} is not an existing directory; using the working directory"
        );
    }
    (normalize(&cwd), BaseDirSource::WorkingDir)
}

/// `path` made absolute against `base` (when relative) and normalized lexically. A leading `~` is expanded
/// to the home directory, since MCP clients pass arguments verbatim without a shell.
pub(crate) fn resolve(base: &Path, path: &str) -> PathBuf {
    absolutize(base, &expand_home(Path::new(path)))
}

/// `path` joined onto `base` when relative, then normalized.
pub(crate) fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&base.join(path))
    }
}

/// Expand a leading `~` / `~/…` to the user's home directory (unchanged when it is unknown).
fn expand_home(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => match home_dir() {
            Some(home) => home.join(components.as_path()),
            None => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Lexical normalization: drop `.` components and fold `..` into the preceding normal component. A `..`
/// directly under the root is dropped (`/..` is `/`); leading `..` of a relative path are kept.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let last_is_normal =
                    matches!(out.components().next_back(), Some(Component::Normal(_)));
                if last_is_normal {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// A `file://` URI for an absolute path, percent-encoding every byte outside the RFC 3986 unreserved set
/// (and the `/` separator), so spaces, `#`, `?`, `%` and non-ASCII names survive.
pub(crate) fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    #[cfg(windows)]
    {
        // C:\Users\me\a b.wav -> file:///C:/Users/me/a%20b.wav (drive colon kept).
        let text = path.to_string_lossy().replace('\\', "/");
        let text = text.strip_prefix("//?/").unwrap_or(&text);
        if !text.starts_with('/') {
            uri.push('/');
        }
        for (i, byte) in text.bytes().enumerate() {
            if byte == b':' && i == 1 {
                uri.push(':');
            } else {
                push_encoded(&mut uri, byte);
            }
        }
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        for &byte in path.as_os_str().as_bytes() {
            push_encoded(&mut uri, byte);
        }
    }
    uri
}

fn push_encoded(out: &mut String, byte: u8) {
    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
        out.push(char::from(byte));
    } else {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        out.push('%');
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0F)]));
    }
}

/// The final component of a path as text (for resource-link names).
pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_dots() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("../b"));
        assert_eq!(normalize(Path::new("./")), PathBuf::from("."));
        assert_eq!(normalize(Path::new("/a/b/")), PathBuf::from("/a/b"));
    }

    #[test]
    fn resolve_relative_and_absolute() {
        let base = Path::new("/proj/dir");
        assert_eq!(resolve(base, "t.yaml"), PathBuf::from("/proj/dir/t.yaml"));
        assert_eq!(
            resolve(base, "../x/t.yaml"),
            PathBuf::from("/proj/x/t.yaml")
        );
        assert_eq!(resolve(base, "/abs/t.yaml"), PathBuf::from("/abs/t.yaml"));
    }

    #[test]
    fn resolve_expands_home() {
        if let Some(home) = home_dir() {
            assert_eq!(
                resolve(Path::new("/x"), "~/a.yaml"),
                normalize(&home.join("a.yaml"))
            );
            assert_eq!(resolve(Path::new("/x"), "~"), normalize(&home));
        }
        // `~user` is not expanded.
        assert_eq!(
            resolve(Path::new("/x"), "~bob/a"),
            PathBuf::from("/x/~bob/a")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(file_uri(Path::new("/tmp/out.wav")), "file:///tmp/out.wav");
        assert_eq!(
            file_uri(Path::new("/tmp/my episode #1/ü?.mp3")),
            "file:///tmp/my%20episode%20%231/%C3%BC%3F.mp3"
        );
        assert_eq!(file_uri(Path::new("/a/100%.wav")), "file:///a/100%25.wav");
    }

    #[test]
    fn default_base_dir_precedence() {
        let tmp = std::env::temp_dir();
        let (dir, source) = default_base_dir(Some(&tmp), Some(OsStr::new("/nonexistent")));
        assert_eq!((dir, source), (normalize(&tmp), BaseDirSource::Option));

        let (dir, source) = default_base_dir(None, Some(tmp.as_os_str()));
        assert_eq!((dir, source), (normalize(&tmp), BaseDirSource::ProjectEnv));

        let cwd = normalize(&std::env::current_dir().unwrap());
        let (dir, source) = default_base_dir(None, Some(OsStr::new("/definitely/not/here")));
        assert_eq!((dir, source), (cwd.clone(), BaseDirSource::WorkingDir));
        let (dir, source) = default_base_dir(None, Some(OsStr::new("")));
        assert_eq!((dir, source), (cwd.clone(), BaseDirSource::WorkingDir));
        let (dir, source) = default_base_dir(Some(Path::new("")), None);
        assert_eq!((dir, source), (cwd, BaseDirSource::WorkingDir));
    }
}
