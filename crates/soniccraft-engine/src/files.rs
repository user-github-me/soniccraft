//! Output files of a render: where they go (DESIGN.md §2.3), what may be replaced, collision and
//! confinement checks, and the two-phase commit that publishes them.
//!
//! # Two-phase commit
//!
//! A render never writes an output file in place. Every file (the audio, subtitles, words, transcript,
//! stems, manifest, preview) is first written to a hidden temporary file next to its destination
//! ([`Staging`]); only after every stage succeeded and the last cancellation check passed are they all
//! renamed into place ([`Staging::commit`]). A failed or cancelled render deletes its temporary files and
//! the directories it created (when empty), and nothing else: outputs of an earlier render survive
//! untouched, and a concurrent render's files are never deleted. A commit that fails half-way (a rename
//! refused) puts back the files it had already replaced.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::{codes, display_path};
use soniccraft_core::ir::{Program, SubtitleFormat};

use crate::RenderOverrides;
use crate::context::{Session, absolute};

/// Which side files a render writes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Exports {
    /// Subtitle formats, in document order.
    pub subtitles: Vec<SubtitleFormat>,
    /// One 32-bit float WAV per track.
    pub stems: bool,
    /// `<stem>.manifest.json`.
    pub manifest: bool,
    /// `<stem>.preview.html`.
    pub preview: bool,
}

impl Exports {
    /// The document's export settings with the CLI/MCP overrides applied. `export_subtitles: true` keeps
    /// the document's formats, or uses vtt + srt when it lists none.
    pub(crate) fn resolve(program: &Program, overrides: &RenderOverrides) -> Exports {
        let output = &program.output;
        let subtitles = match overrides.export_subtitles {
            Some(false) => Vec::new(),
            Some(true) if output.subtitle_formats.is_empty() => {
                vec![SubtitleFormat::Vtt, SubtitleFormat::Srt]
            }
            _ => output.subtitle_formats.clone(),
        };
        Exports {
            subtitles,
            stems: overrides.export_stems.unwrap_or(output.export_stems),
            manifest: overrides.export_manifest.unwrap_or(output.export_manifest),
            preview: overrides.preview,
        }
    }
}

/// The main output: `RenderOverrides::output_path` (relative to the current directory) > `output.path`
/// (relative to the timeline directory) > `<timeline dir>/<timeline stem>.<format>`.
pub(crate) fn audio_path(
    session: &Session,
    program: &Program,
    overrides: &RenderOverrides,
) -> PathBuf {
    if let Some(path) = &overrides.output_path {
        return absolute(path);
    }
    if let Some(path) = &program.output.path {
        return absolute(&session.base_dir.join(path));
    }
    session.base_dir.join(format!(
        "{}.{}",
        session.stem,
        program.output.format.as_str()
    ))
}

/// Why an output plan was refused: a message and a mechanical fix.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutputProblem {
    pub message: String,
    pub hint: String,
}

impl OutputProblem {
    fn new(message: impl Into<String>, hint: impl Into<String>) -> Self {
        OutputProblem {
            message: message.into(),
            hint: hint.into(),
        }
    }
}

/// The generic fix of a refused output path.
const OTHER_PATH_HINT: &str =
    "Choose another output path (CLI `-o`, MCP `output_path`, or `output.path`)";

/// Every file a render will write.
#[derive(Debug, Clone)]
pub(crate) struct OutputPlan {
    /// The audio file.
    pub audio: PathBuf,
    /// Its directory; side files live here.
    pub dir: PathBuf,
    /// Subtitle files.
    pub subtitles: Vec<(SubtitleFormat, PathBuf)>,
    /// `<stem>.stems/` and one file per track (index = track), when stems are exported.
    pub stems: Option<(PathBuf, Vec<PathBuf>)>,
    /// The manifest.
    pub manifest: Option<PathBuf>,
    /// The web preview.
    pub preview: Option<PathBuf>,
}

impl OutputPlan {
    /// Plan the side files of `audio` (all derived from its file stem) and check that no output collides
    /// with another output, the timeline file or an input asset, and that nothing in the way is a file
    /// SonicCraft did not write (an existing audio output is replaced; `notes.txt` or a directory is not).
    pub(crate) fn new(
        audio: PathBuf,
        exports: &Exports,
        program: &Program,
        timeline: Option<&Path>,
    ) -> Result<OutputPlan, OutputProblem> {
        if audio.is_dir() {
            return Err(OutputProblem::new(
                format!(
                    "the output path `{}` is a directory",
                    display_path(&audio.display().to_string())
                ),
                format!(
                    "Give a file name inside it, such as `{}`",
                    display_path(&audio.join("episode.wav").display().to_string())
                ),
            ));
        }
        let name = audio
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                OutputProblem::new(
                    format!("the output path `{}` has no file name", audio.display()),
                    "Give the output a file name such as `episode.wav`",
                )
            })?;
        let stem = audio
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(name);
        let dir = audio
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let side = |suffix: &str| dir.join(format!("{stem}{suffix}"));
        let subtitles = exports
            .subtitles
            .iter()
            .map(|&f| (f, side(subtitle_suffix(f))))
            .collect();
        let stems = exports.stems.then(|| {
            let stems_dir = side(".stems");
            let files = stem_file_names(program)
                .into_iter()
                .map(|n| stems_dir.join(n))
                .collect();
            (stems_dir, files)
        });
        let plan = OutputPlan {
            subtitles,
            stems,
            manifest: exports.manifest.then(|| side(".manifest.json")),
            preview: exports.preview.then(|| side(".preview.html")),
            dir,
            audio,
        };
        plan.check_collisions(program, timeline)?;
        plan.check_replaceable()?;
        Ok(plan)
    }

    /// Every top-level path the plan writes (stem files excluded: they live in their own directory).
    fn paths(&self) -> Vec<(&'static str, &Path)> {
        let mut out: Vec<(&'static str, &Path)> = vec![("the audio output", &self.audio)];
        for (format, path) in &self.subtitles {
            out.push((subtitle_label(*format), path));
        }
        if let Some((dir, _)) = &self.stems {
            out.push(("the stems directory", dir));
        }
        if let Some(path) = &self.manifest {
            out.push(("the manifest", path));
        }
        if let Some(path) = &self.preview {
            out.push(("the preview", path));
        }
        out
    }

    fn check_collisions(
        &self,
        program: &Program,
        timeline: Option<&Path>,
    ) -> Result<(), OutputProblem> {
        let paths = self.paths();
        for (i, (label, path)) in paths.iter().enumerate() {
            let shown = display_path(&path.display().to_string());
            if let Some((other, _)) = paths[..i].iter().find(|(_, p)| same_file(p, path)) {
                return Err(OutputProblem::new(
                    format!("{label} and {other} would both be written to `{shown}`"),
                    OTHER_PATH_HINT,
                ));
            }
            if let Some(timeline) = timeline
                && same_file(path, timeline)
            {
                return Err(OutputProblem::new(
                    format!(
                        "{label} would overwrite the timeline file `{}`",
                        display_path(&timeline.display().to_string())
                    ),
                    OTHER_PATH_HINT,
                ));
            }
            if let Some((_, _, clip)) = program
                .clips()
                .find(|(_, _, clip)| same_file(path, &clip.source_path))
            {
                return Err(OutputProblem::new(
                    format!("{label} would overwrite the audio asset `{}`", clip.source),
                    OTHER_PATH_HINT,
                ));
            }
        }
        if let Some((_, files)) = &self.stems {
            for file in files {
                if let Some((_, _, clip)) = program
                    .clips()
                    .find(|(_, _, clip)| same_file(file, &clip.source_path))
                {
                    return Err(OutputProblem::new(
                        format!(
                            "a stem file would overwrite the audio asset `{}`",
                            clip.source
                        ),
                        OTHER_PATH_HINT,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Nothing the render would replace is something else than an output of its kind: the audio slot
    /// takes a file with an audio extension, the side files a file (their names are SonicCraft's own),
    /// the stems a directory.
    fn check_replaceable(&self) -> Result<(), OutputProblem> {
        check_slot(&self.audio, Slot::Audio)?;
        for (_, path) in &self.subtitles {
            check_slot(path, Slot::SideFile)?;
        }
        for path in [&self.manifest, &self.preview].into_iter().flatten() {
            check_slot(path, Slot::SideFile)?;
        }
        if let Some((dir, files)) = &self.stems {
            check_slot(dir, Slot::Directory)?;
            for file in files {
                check_slot(file, Slot::SideFile)?;
            }
        }
        Ok(())
    }

    /// Every directory the plan writes into.
    pub(crate) fn directories(&self) -> Vec<&Path> {
        let mut dirs = vec![self.dir.as_path()];
        if let Some((stems, _)) = &self.stems {
            dirs.push(stems);
        }
        dirs
    }

    /// `path` relative to the output directory, with `/` separators (manifest and preview links).
    pub(crate) fn relative(&self, path: &Path) -> String {
        match path.strip_prefix(&self.dir) {
            Ok(rel) => rel
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// What may already exist where a render writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// The audio output: a file with an audio extension (a previous render's output).
    Audio,
    /// A side file (`.vtt`, `.srt`, `.words.json`, `.transcript.json`, `.manifest.json`, `.preview.html`,
    /// a stem): any file, its name is SonicCraft's own.
    SideFile,
    /// The stems directory.
    Directory,
}

/// Extensions of files the audio slot may replace.
const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "mp4", "aif", "aiff",
];

fn check_slot(path: &Path, slot: Slot) -> Result<(), OutputProblem> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(()); // nothing there (or unreadable: the write reports it)
    };
    let shown = display_path(&path.display().to_string());
    // A symlink is replaced, never written through; judge it by what it points at all the same.
    let is_dir = meta.is_dir() || (meta.is_symlink() && path.is_dir());
    let delete_hint =
        |what: &str| format!("{OTHER_PATH_HINT}, or delete or rename `{shown}` first ({what})");
    match slot {
        Slot::Directory if is_dir => Ok(()),
        Slot::Directory => Err(OutputProblem::new(
            format!(
                "refusing to overwrite `{shown}`: it is a file, not a SonicCraft stems directory"
            ),
            delete_hint("stems are written into a directory of that name"),
        )),
        _ if is_dir => Err(OutputProblem::new(
            format!("refusing to overwrite `{shown}`: it is a directory, not a SonicCraft output"),
            delete_hint("SonicCraft replaces only files it writes itself"),
        )),
        Slot::Audio => {
            let audio = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
            if audio {
                Ok(())
            } else {
                Err(OutputProblem::new(
                    format!("refusing to overwrite `{shown}`: it is not a SonicCraft output"),
                    format!(
                        "Choose an output path with an audio extension (e.g. `episode.wav`) or a file that does \
                         not exist yet, or delete or rename `{shown}` first"
                    ),
                ))
            }
        }
        Slot::SideFile => Ok(()),
    }
}

/// `invalid_value` at `output.path` when the output path written in the document, or its stems directory,
/// resolves outside `dir` (`..` components or symlinks), for engines that confine document output paths
/// ([`crate::EngineOptions::confine_outputs_to`]).
pub(crate) fn document_output_escape(
    program: &Program,
    base_dir: &Path,
    dir: &Path,
) -> Option<Diagnostic> {
    let written = program.output.path.as_deref()?;
    let audio = absolute(&base_dir.join(written));
    let stems = program.output.export_stems.then(|| {
        let stem = audio
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        audio.with_file_name(format!("{stem}.stems"))
    });
    let escapes = |p: &Path| !is_inside(p, dir);
    if escapes(&audio) || stems.as_deref().is_some_and(escapes) {
        return Some(confinement_error(written, base_dir, dir));
    }
    None
}

/// The `invalid_value` diagnostic of an output path written in the document that leaves `dir`.
/// `timeline_dir` is the directory relative paths of the document resolve against.
pub(crate) fn confinement_error(written: &str, timeline_dir: &Path, dir: &Path) -> Diagnostic {
    // A timeline outside `dir` cannot fix this with a relative path: every one resolves next to it.
    let hint = if is_inside(timeline_dir, dir) {
        "Use a relative path inside that directory, without `..` and symlinks that lead out of it (e.g. \
         `output.path: episode.wav`), or pass an explicit output path (MCP `output_path`) to write elsewhere"
    } else {
        "The timeline itself is outside that directory, so a relative `output.path` resolves outside it too: \
         pass an explicit output path (MCP `output_path`), or a `base_dir` that contains the timeline"
    };
    Diagnostic::error(
        codes::INVALID_VALUE,
        format!(
            "`output.path` (`{written}`) resolves outside {}, the directory this server writes timeline \
             outputs into",
            dir.display()
        ),
    )
    .with_path("output.path")
    .with_hint(hint)
}

/// True when `path` (absolute) resolves inside `dir`, following the symlinks of every existing ancestor.
pub(crate) fn is_inside(path: &Path, dir: &Path) -> bool {
    let root = resolve(&absolute(dir));
    resolve(&absolute(path)).starts_with(&root)
}

/// `path` with the symlinks of its existing ancestors (and of itself) resolved; components below the
/// deepest existing directory are appended as written, with `..` removing the previous component.
fn resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
            }
        }
    }
    out
}

/// Side-file suffix of a subtitle format.
fn subtitle_suffix(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Vtt => ".vtt",
        SubtitleFormat::Srt => ".srt",
        SubtitleFormat::Json => ".words.json",
        SubtitleFormat::Transcript => ".transcript.json",
    }
}

fn subtitle_label(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Vtt => "the WebVTT subtitles",
        SubtitleFormat::Srt => "the SRT subtitles",
        SubtitleFormat::Json => "the words.json export",
        SubtitleFormat::Transcript => "the transcript export",
    }
}

/// Stem file names per track: `<track_id>.wav`, with `:` (not allowed on Windows) replaced by `_` and a
/// numeric suffix when two names would collide (case-insensitively).
fn stem_file_names(program: &Program) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    program
        .tracks
        .iter()
        .map(|track| {
            let base: String = track
                .id
                .chars()
                .map(|c| if c == ':' { '_' } else { c })
                .collect();
            let mut name = format!("{base}.wav");
            let mut n = 2;
            while !used.insert(name.to_lowercase()) {
                name = format!("{base}-{n}.wav");
                n += 1;
            }
            name
        })
        .collect()
}

/// True when `a` and `b` name the same file: equal paths, or the same canonical path when `a` exists
/// (catches symlinks and case-insensitive file systems).
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Process-wide counter, hashed into every temporary name.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 16 hex digits nobody can predict (SipHash of a counter under the process's random keys); temporary
/// names only, nothing random reaches an output.
fn unpredictable() -> String {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:016x}",
        RandomState::new().hash_one((std::process::id(), n))
    )
}

/// A hidden name next to `target` for one of its temporary files: `.<name>.<random>.<kind>` plus the
/// target's extension, so decoders that look at extensions (the lossy-format check) read it correctly.
fn sibling(target: &Path, kind: &str) -> Option<PathBuf> {
    let name = target.file_name()?.to_string_lossy().into_owned();
    let ext = target
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    Some(target.with_file_name(format!(".{name}.{}.{kind}{ext}", unpredictable())))
}

/// One file written to a temporary name, waiting for the commit.
#[derive(Debug)]
struct Staged {
    temp: PathBuf,
    target: PathBuf,
}

/// The files of one render, written under temporary names until [`Staging::commit`] (see the module
/// docs). Dropping a staging that was not committed removes everything it wrote.
#[derive(Debug, Default)]
pub(crate) struct Staging {
    staged: Vec<Staged>,
    /// Directories this render created, outermost first.
    created_dirs: Vec<PathBuf>,
    committed: bool,
}

/// Why [`Staging::commit`] failed: the file it could not put in place, and the error. Every file was
/// left as it was before the commit.
#[derive(Debug)]
pub(crate) struct CommitError {
    pub target: PathBuf,
    pub error: std::io::Error,
}

impl Staging {
    /// `create_dir_all`, remembering the directories that did not exist yet (removed on abort when
    /// empty).
    pub(crate) fn create_dir_all(&mut self, dir: &Path) -> std::io::Result<()> {
        let mut missing = Vec::new();
        let mut current = Some(dir);
        while let Some(d) = current {
            if d.as_os_str().is_empty() || d.exists() {
                break;
            }
            missing.push(d.to_path_buf());
            current = d.parent();
        }
        std::fs::create_dir_all(dir)?;
        for d in missing.into_iter().rev() {
            if !self.created_dirs.contains(&d) {
                self.created_dirs.push(d);
            }
        }
        Ok(())
    }

    /// Create the temporary file of `target` (exclusively: never through an existing file or symlink),
    /// open for writing. A target staged before is staged again (the old temporary file is removed).
    pub(crate) fn create(&mut self, target: &Path) -> std::io::Result<(PathBuf, File)> {
        self.discard(target);
        let mut last = None;
        for _ in 0..16 {
            let temp = sibling(target, "tmp").ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "{} has no file name",
                        display_path(&target.display().to_string())
                    ),
                )
            })?;
            match OpenOptions::new().write(true).create_new(true).open(&temp) {
                Ok(file) => {
                    self.staged.push(Staged {
                        temp: temp.clone(),
                        target: target.to_path_buf(),
                    });
                    return Ok((temp, file));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no free temporary name")))
    }

    /// Reserve the temporary path of `target` for a writer that replaces it by renaming its own file
    /// over it (the encoders).
    pub(crate) fn reserve(&mut self, target: &Path) -> std::io::Result<PathBuf> {
        self.create(target).map(|(temp, _)| temp)
    }

    /// Write `bytes` as the new contents of `target`.
    pub(crate) fn write(&mut self, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let (_, mut file) = self.create(target)?;
        let result = file.write_all(bytes).and_then(|()| file.flush());
        drop(file);
        if result.is_err() {
            self.discard(target);
        }
        result
    }

    /// Forget `target` and delete its temporary file.
    pub(crate) fn discard(&mut self, target: &Path) {
        self.staged.retain(|s| {
            if s.target == target {
                let _ = std::fs::remove_file(&s.temp);
                false
            } else {
                true
            }
        });
    }

    /// Rename every staged file over its target. On a failure every target is restored to what it was
    /// before the commit, the temporary files are removed, and the error names the file.
    pub(crate) fn commit(&mut self) -> Result<(), CommitError> {
        let staged = std::mem::take(&mut self.staged);
        // (target, backup of the file it replaced)
        let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::with_capacity(staged.len());
        let mut failure = None;
        for (i, s) in staged.iter().enumerate() {
            match replace(&s.temp, &s.target) {
                Ok(backup) => done.push((s.target.clone(), backup)),
                Err(error) => {
                    failure = Some((
                        i,
                        CommitError {
                            target: s.target.clone(),
                            error,
                        },
                    ));
                    break;
                }
            }
        }
        match failure {
            None => {
                for backup in done.into_iter().filter_map(|(_, b)| b) {
                    let _ = std::fs::remove_file(backup);
                }
                self.committed = true;
                Ok(())
            }
            Some((failed, error)) => {
                for (target, backup) in done.into_iter().rev() {
                    match backup {
                        Some(backup) => {
                            let _ = std::fs::rename(backup, &target);
                        }
                        None => {
                            let _ = std::fs::remove_file(&target);
                        }
                    }
                }
                for s in &staged[failed..] {
                    let _ = std::fs::remove_file(&s.temp);
                }
                self.remove_created_dirs();
                self.committed = true; // nothing left to clean up
                Err(error)
            }
        }
    }

    /// Delete every temporary file and the created directories that are empty (failed or cancelled
    /// renders).
    pub(crate) fn abort(&mut self) {
        for s in self.staged.drain(..) {
            if let Err(e) = std::fs::remove_file(&s.temp)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("could not remove {}: {e}", s.temp.display());
            }
        }
        self.remove_created_dirs();
        self.committed = true;
    }

    fn remove_created_dirs(&mut self) {
        for dir in self.created_dirs.drain(..).rev() {
            // Only empty directories are removed; anything else in them is not ours.
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            self.abort();
        }
    }
}

/// Put `temp` in place of `target`. An existing target is kept as a hidden backup (a hard link, so the
/// target is replaced atomically, or a rename where links are not supported) and its path returned; the
/// caller deletes it after a successful commit or renames it back on a failure.
fn replace(temp: &Path, target: &Path) -> std::io::Result<Option<PathBuf>> {
    let backup = match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
        // Checked when the render started; something else was put there since.
        Ok(meta) if meta.is_dir() => {
            return Err(std::io::Error::other(format!(
                "`{}` is a directory now",
                display_path(&target.display().to_string())
            )));
        }
        Ok(_) => {
            let backup = sibling(target, "bak").ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name")
            })?;
            if std::fs::hard_link(target, &backup).is_err() {
                std::fs::rename(target, &backup)?;
            }
            Some(backup)
        }
    };
    match std::fs::rename(temp, target) {
        Ok(()) => Ok(backup),
        Err(e) => {
            if let Some(backup) = &backup {
                if std::fs::symlink_metadata(target).is_ok() {
                    let _ = std::fs::remove_file(backup);
                } else {
                    let _ = std::fs::rename(backup, target);
                }
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn staged_files_appear_only_on_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("out/a.wav");
        let b = tmp.path().join("out/a.vtt");
        let mut staging = Staging::default();
        staging.create_dir_all(a.parent().unwrap()).unwrap();
        staging.write(&a, b"new audio").unwrap();
        staging.write(&b, b"new vtt").unwrap();
        assert!(!a.exists() && !b.exists());
        assert_eq!(
            names(a.parent().unwrap()).len(),
            2,
            "two hidden temporary files"
        );
        staging.commit().unwrap();
        assert_eq!((read(&a), read(&b)), ("new audio".into(), "new vtt".into()));
        assert_eq!(names(a.parent().unwrap()), ["a.vtt", "a.wav"]);
    }

    #[test]
    fn abort_keeps_existing_outputs_and_removes_what_it_created() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old.wav");
        std::fs::write(&old, b"previous render").unwrap();
        let mut staging = Staging::default();
        staging.write(&old, b"half a render").unwrap();
        let deep = tmp.path().join("x/y");
        staging.create_dir_all(&deep).unwrap();
        staging.write(&deep.join("z.vtt"), b"z").unwrap();
        std::fs::write(tmp.path().join("x/foreign.txt"), b"not ours").unwrap();
        drop(staging); // dropping an uncommitted staging aborts it
        assert_eq!(read(&old), "previous render");
        assert!(!deep.exists(), "created and empty: removed");
        assert!(
            tmp.path().join("x/foreign.txt").exists(),
            "foreign files survive"
        );
        assert_eq!(names(tmp.path()), ["old.wav", "x"]);
    }

    #[test]
    fn a_failed_commit_restores_every_replaced_file() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("ep.wav");
        let second = tmp.path().join("ep.srt");
        std::fs::write(&first, b"old wav").unwrap();
        let mut staging = Staging::default();
        staging.write(&first, b"new wav").unwrap();
        staging.write(&second, b"new srt").unwrap();
        // Something puts a directory where the second file goes after the render started.
        std::fs::create_dir(&second).unwrap();
        std::fs::write(second.join("keep"), b"x").unwrap();
        let error = staging.commit().unwrap_err();
        assert_eq!(error.target, second);
        assert_eq!(read(&first), "old wav", "the first file was put back");
        assert!(second.join("keep").exists());
        assert_eq!(
            names(tmp.path()),
            ["ep.srt", "ep.wav"],
            "no temporary or backup files"
        );
    }

    #[test]
    fn existing_files_are_replaced_only_by_outputs_of_their_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let notes = tmp.path().join("notes.txt");
        std::fs::write(&notes, b"keep me").unwrap();
        let err = check_slot(&notes, Slot::Audio).unwrap_err();
        assert_eq!(
            err.message,
            format!(
                "refusing to overwrite `{}`: it is not a SonicCraft output",
                display_path(&notes.display().to_string())
            )
        );
        assert!(err.hint.contains("audio extension"), "{}", err.hint);
        let wav = tmp.path().join("ep.WAV");
        std::fs::write(&wav, b"old").unwrap();
        assert!(check_slot(&wav, Slot::Audio).is_ok());
        let dir = tmp.path().join("ep.srt");
        std::fs::create_dir(&dir).unwrap();
        assert!(
            check_slot(&dir, Slot::SideFile)
                .unwrap_err()
                .message
                .contains("it is a directory")
        );
        assert!(check_slot(&dir, Slot::Directory).is_ok());
        assert!(
            check_slot(&notes, Slot::Directory)
                .unwrap_err()
                .message
                .contains("stems directory")
        );
        assert!(check_slot(&tmp.path().join("missing.txt"), Slot::Audio).is_ok());
    }

    #[test]
    fn confinement_follows_dot_dots_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        assert!(is_inside(&base.join("a.wav"), &base));
        assert!(is_inside(&base.join("new/dir/a.wav"), &base));
        assert!(is_inside(&base.join("sub/../a.wav"), &base));
        assert!(!is_inside(&base.join("../outside/a.wav"), &base));
        assert!(!is_inside(&base.join("missing/../../outside/a.wav"), &base));
        assert!(!is_inside(Path::new("/etc/passwd"), &base));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();
            assert!(!is_inside(&base.join("link/a.wav"), &base));
            assert!(!is_inside(&base.join("link/new/a.wav"), &base));
            std::os::unix::fs::symlink(&base, tmp.path().join("alias")).unwrap();
            assert!(is_inside(&tmp.path().join("alias/a.wav"), &base));
        }
    }
}
