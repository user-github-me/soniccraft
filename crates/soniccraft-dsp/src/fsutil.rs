//! Filesystem helpers: atomic writes (temp file in the destination directory + rename) and scratch
//! files.
//!
//! Temp and scratch files are created with `create_new` (`O_CREAT | O_EXCL`) under a name with 64
//! unpredictable bits. Exclusive creation never follows a symlink planted at that name (it fails, and
//! another name is tried), so a writable shared directory cannot redirect a write elsewhere. Writers
//! that need a path instead of an open file (ffmpeg) get a path inside a private directory created the
//! same way (mode `0700` on Unix), where nobody else can plant anything. The final `rename` is atomic
//! as before.

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::DspError;

/// Process-wide counter, hashed into every temp name.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Names tried before giving up when every candidate already exists.
const MAX_ATTEMPTS: usize = 32;

/// 16 hex digits nobody can predict: SipHash of a counter under the process's random `RandomState`
/// keys (seeded from the operating system's RNG). Only temp names use this; nothing random reaches a
/// rendered file.
fn unpredictable_suffix() -> String {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let h = RandomState::new().hash_one((std::process::id(), n));
    format!("{h:016x}")
}

/// The directory and file name of `path` (a bare file name lives in the current directory).
fn split(path: &Path) -> Result<(&Path, String), DspError> {
    let name = path.file_name().ok_or_else(|| {
        DspError::InvalidInput(format!("output path {} has no file name", path.display()))
    })?;
    let dir = path.parent().unwrap_or(Path::new(""));
    Ok((dir, name.to_string_lossy().into_owned()))
}

/// Run `create` on fresh names `dir/<prefix><random><suffix>` until one does not exist yet.
fn create_unique<T>(
    dir: &Path,
    prefix: &str,
    suffix: &str,
    create: impl Fn(&Path) -> std::io::Result<T>,
) -> Result<(PathBuf, T), DspError> {
    let mut last = None;
    for _ in 0..MAX_ATTEMPTS {
        let path = dir.join(format!("{prefix}{}{suffix}", unpredictable_suffix()));
        match create(&path) {
            Ok(made) => return Ok((path, made)),
            // Taken (possibly by a planted symlink, which exclusive creation never follows).
            Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some((path, e)),
            Err(e) => return Err(DspError::io(path, e)),
        }
    }
    let (path, e) = last.expect("MAX_ATTEMPTS > 0");
    Err(DspError::io(path, e))
}

/// Create a new file exclusively (never through an existing file or symlink).
fn create_new_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Create a new directory only its owner can use (`mkdir` fails on any existing name, symlink or not).
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Removes the file (and then its private directory) on drop unless disarmed.
struct RemoveOnDrop {
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
}

impl RemoveOnDrop {
    fn disarm(&mut self) {
        self.file = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        // Best effort: the file may not exist if the writer failed before creating it.
        if let Some(file) = self.file.take() {
            let _ = std::fs::remove_file(file);
        }
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Create the parent directory of `path` if it does not exist.
pub(crate) fn ensure_parent_dir(path: &Path) -> Result<(), DspError> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            std::fs::create_dir_all(dir).map_err(|e| DspError::io(dir, e))
        }
        _ => Ok(()),
    }
}

/// Rename the finished temp file over `path` and return its size.
fn commit(temp: &Path, path: &Path, guard: &mut RemoveOnDrop) -> Result<u64, DspError> {
    let bytes = std::fs::metadata(temp)
        .map_err(|e| DspError::io(temp, e))?
        .len();
    std::fs::rename(temp, path).map_err(|e| DspError::io(path, e))?;
    guard.disarm();
    Ok(bytes)
}

/// Write a file atomically through a buffered writer: the temp file is created exclusively next to
/// `path` (see the module docs), handed to `write`, flushed and renamed over `path`. On any error the
/// temp file is removed and `path` is left untouched. Missing parent directories are created.
///
/// Returns the size of the final file in bytes.
///
/// The data is not `fsync`ed: the rename guarantees that readers never observe a partially written
/// file, which is what caches and outputs need; durability across power loss is not a goal.
pub(crate) fn write_atomic<F>(path: &Path, write: F) -> Result<u64, DspError>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<(), DspError>,
{
    ensure_parent_dir(path)?;
    let (dir, name) = split(path)?;
    let (temp, file) = create_unique(dir, &format!(".{name}."), ".tmp", create_new_file)?;
    let mut guard = RemoveOnDrop {
        file: Some(temp.clone()),
        dir: None,
    };
    let mut writer = BufWriter::with_capacity(1 << 16, file);
    write(&mut writer)?;
    writer.flush().map_err(|e| DspError::io(&temp, e))?;
    // Close the file before renaming it (required on Windows).
    drop(writer);
    commit(&temp, path, &mut guard)
}

/// Like [`write_atomic`] for writers that create the file themselves: `write` receives a path inside a
/// fresh private directory next to `path` and must create the file there; on success it is renamed over
/// `path` and the directory is removed.
pub(crate) fn write_atomic_path<F>(path: &Path, write: F) -> Result<u64, DspError>
where
    F: FnOnce(&Path) -> Result<(), DspError>,
{
    ensure_parent_dir(path)?;
    let (dir, name) = split(path)?;
    let (private, ()) = create_unique(dir, &format!(".{name}."), ".tmp", create_private_dir)?;
    let temp = private.join(&name);
    let mut guard = RemoveOnDrop {
        file: Some(temp.clone()),
        dir: Some(private),
    };
    write(&temp)?;
    commit(&temp, path, &mut guard)
}

/// Write `data` to `path` atomically. Returns the number of bytes written.
pub(crate) fn write_bytes_atomic(path: &Path, data: &[u8]) -> Result<u64, DspError> {
    write_atomic(path, |w| {
        w.write_all(data).map_err(|e| DspError::io(path, e))
    })
}

/// A scratch file in the system temp directory that is deleted on drop.
pub(crate) struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    /// Create a scratch file containing `data` (exclusively, under an unpredictable name). `extension`
    /// (without the dot) is appended to the name so tools that sniff by extension (ffmpeg) get a hint.
    pub(crate) fn with_contents(data: &[u8], extension: Option<&str>) -> Result<Self, DspError> {
        let suffix = extension
            .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let (path, mut file) = create_unique(
            &std::env::temp_dir(),
            "soniccraft-",
            &suffix,
            create_new_file,
        )?;
        let scratch = ScratchFile { path };
        file.write_all(data)
            .map_err(|e| DspError::io(&scratch.path, e))?;
        Ok(scratch)
    }

    /// The scratch file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("out.bin");
        assert_eq!(write_bytes_atomic(&path, b"hello").unwrap(), 5);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(write_bytes_atomic(&path, b"hi").unwrap(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");
        let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1, "no temp files left behind");
    }

    #[test]
    fn failed_write_leaves_destination_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        std::fs::write(&path, b"original").unwrap();
        let err = write_atomic(&path, |w| {
            w.write_all(b"partial").unwrap();
            Err(DspError::InvalidInput("boom".into()))
        });
        assert!(err.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "temp file removed");
        // The path-based variant cleans up its private directory too.
        let err = write_atomic_path(&path, |temp| {
            std::fs::write(temp, b"partial").unwrap();
            Err(DspError::InvalidInput("boom".into()))
        });
        assert!(err.is_err());
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "private directory removed");
    }

    #[test]
    fn path_writer_gets_a_private_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.m4a");
        let bytes = write_atomic_path(&path, |temp| {
            assert_eq!(temp.file_name().unwrap(), "x.m4a");
            let private = temp.parent().unwrap();
            assert_ne!(private, dir.path());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(private).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "only the owner can enter it");
            }
            std::fs::write(temp, b"abc").map_err(|e| DspError::io(temp, e))
        })
        .unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "private directory removed");
    }

    #[test]
    fn temp_names_are_unpredictable_and_unique() {
        let names: std::collections::HashSet<String> =
            (0..1000).map(|_| unpredictable_suffix()).collect();
        assert_eq!(names.len(), 1000);
        assert!(
            names
                .iter()
                .all(|n| n.len() == 16 && n.chars().all(|c| c.is_ascii_hexdigit()))
        );
    }

    /// A symlink planted at a candidate temp name is never followed: creation fails, another name is
    /// used, and the symlink target is untouched.
    #[cfg(unix)]
    #[test]
    fn planted_symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"precious").unwrap();
        let link = dir.path().join(".out.wav.planted.tmp");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        // Exclusive creation refuses the symlink itself.
        let err = create_new_file(&link).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        // A colliding candidate is skipped: the first name is taken by the symlink, the second is free.
        let attempts = std::cell::Cell::new(0);
        let (made, _) = create_unique(dir.path(), ".out.wav.", ".tmp", |p| {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                create_new_file(&link)
            } else {
                create_new_file(p)
            }
        })
        .unwrap();
        assert_eq!(attempts.get(), 2);
        assert_ne!(made, link);
        let out = dir.path().join("out.wav");
        write_bytes_atomic(&out, b"RIFF").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert_eq!(std::fs::read(&out).unwrap(), b"RIFF");
        assert!(
            !std::fs::symlink_metadata(&out)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn scratch_file_is_removed() {
        let p;
        {
            let s = ScratchFile::with_contents(b"abc", Some("mp3")).unwrap();
            p = s.path().to_path_buf();
            assert!(p.exists());
            assert_eq!(p.extension().unwrap(), "mp3");
            assert_eq!(std::fs::read(&p).unwrap(), b"abc");
        }
        assert!(!p.exists());
    }
}
