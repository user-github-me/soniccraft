//! SonicCraft engine: orchestrates parsing, validation, TTS synthesis, layout, mixing, mastering, encoding
//! and side-file export (docs/DESIGN.md §2–§8, §10). This is the API the CLI and the MCP server use.
//!
//! - [`Engine::validate`] compiles a timeline and checks the environment it would render in: audio
//!   assets exist and decode, providers are usable (credentials, binaries, platform), ffmpeg is present
//!   when the output format needs it, cue texts fit the provider's request limit.
//! - [`Engine::plan`] is a dry run: it lays the timeline out with cached or estimated durations and never
//!   calls a TTS provider.
//! - [`Engine::render`] runs the whole signal chain (DESIGN.md §4) and writes the output file plus the
//!   requested side files (subtitles, stems, manifest, web preview).
//!
//! Every problem is reported as a structured [`Diagnostic`] with a stable code, a document path (with line
//! and column when known) and, wherever possible, a mechanical fix in `hint`.
//!
//! All operations are `async` and their futures are `Send`. CPU-heavy work (decoding, resampling, mixing,
//! mastering, encoding) runs on Tokio's blocking pool, so the methods must be called inside a Tokio runtime
//! and never stall it. Renders are deterministic: the same timeline, TTS cache and binary produce
//! byte-identical audio (wav, flac, mp3), subtitles and manifests.

#![warn(missing_docs)]

mod budget;
mod cache;
mod cancel;
mod checks;
mod context;
mod diag;
mod files;
mod plan;
mod preview;
mod providers;
mod render;
pub mod report;
mod reports;
mod speech;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use soniccraft_core::SourceFormat;

pub use report::*;
pub use soniccraft_core::{Diagnostic, Severity};
pub use soniccraft_tts::EnvSource;

/// A timeline document plus where it lives.
#[derive(Debug, Clone)]
pub struct TimelineSource {
    /// The document text (YAML or JSON).
    pub text: String,
    /// How to parse `text`.
    pub format: SourceFormat,
    /// The file it was read from, if any.
    pub path: Option<PathBuf>,
    /// Directory relative paths resolve against (the file's directory, or the caller's choice).
    pub base_dir: PathBuf,
}

/// The largest timeline document [`TimelineSource::from_path`] and [`TimelineSource::from_reader`] read:
/// the parser's limit ([`soniccraft_core::parse::MAX_DOCUMENT_BYTES`], 5 MiB).
pub const MAX_TIMELINE_BYTES: usize = soniccraft_core::parse::MAX_DOCUMENT_BYTES;

impl TimelineSource {
    /// Read a timeline file. The base directory is the file's parent directory.
    ///
    /// Only regular files are read (symlinks to them are followed), and never more than
    /// [`MAX_TIMELINE_BYTES`] + 1 bytes, so a device such as `/dev/zero`, a FIFO or a huge file fails at
    /// once instead of being read without bound. Errors, by [`std::io::ErrorKind`]:
    /// - `NotFound` (and the other errors of opening the file);
    /// - `IsADirectory`: the path is a directory;
    /// - `InvalidInput`: it is not a regular file (a FIFO, socket or device), message
    ///   "`<path>` is not a regular file (it is a FIFO); a timeline is a YAML or JSON text file";
    /// - `FileTooLarge`: it is larger than [`MAX_TIMELINE_BYTES`], message
    ///   "`<path>` is 7.3 MB; a timeline is at most 5 MB (5242880 bytes)";
    /// - `InvalidData`: the text is not UTF-8.
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let text = read_timeline_file(path)?;
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let base_dir = abs
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let format = SourceFormat::from_extension(abs.extension().and_then(|e| e.to_str()));
        Ok(TimelineSource {
            text,
            format,
            path: Some(abs),
            base_dir,
        })
    }

    /// Read a timeline from `reader` (e.g. stdin), at most [`MAX_TIMELINE_BYTES`] + 1 bytes: a longer
    /// document fails with `FileTooLarge` ("the timeline is larger than 5 MB …") as soon as the limit is
    /// passed, without reading the rest. Text that is not UTF-8 fails with `InvalidData`.
    pub fn from_reader(
        reader: impl std::io::Read,
        base_dir: impl Into<PathBuf>,
        format: SourceFormat,
    ) -> std::io::Result<Self> {
        let text = read_bounded(reader).map_err(|e| match e {
            BoundedRead::TooLarge => std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                format!(
                    "the timeline is larger than {} MB; a timeline is at most {MAX_TIMELINE_BYTES} bytes",
                    MAX_TIMELINE_BYTES / (1024 * 1024)
                ),
            ),
            BoundedRead::Io(e) => e,
        })?;
        Ok(TimelineSource::from_text(text, base_dir, format))
    }

    /// An in-memory timeline (e.g. MCP `yaml_content`).
    pub fn from_text(
        text: impl Into<String>,
        base_dir: impl Into<PathBuf>,
        format: SourceFormat,
    ) -> Self {
        TimelineSource {
            text: text.into(),
            format,
            path: None,
            base_dir: base_dir.into(),
        }
    }

    /// File name for messages (`timeline.yaml`, or `<inline>`).
    pub fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<inline>".to_string())
    }
}

/// Why a bounded read stopped.
enum BoundedRead {
    /// More than [`MAX_TIMELINE_BYTES`] bytes.
    TooLarge,
    /// A read error (or text that is not UTF-8, as `InvalidData`).
    Io(std::io::Error),
}

/// Read at most [`MAX_TIMELINE_BYTES`] + 1 bytes of `reader` as UTF-8 text.
fn read_bounded(reader: impl std::io::Read) -> Result<String, BoundedRead> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader
        .take(MAX_TIMELINE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(BoundedRead::Io)?;
    if bytes.len() > MAX_TIMELINE_BYTES {
        return Err(BoundedRead::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| {
        BoundedRead::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the timeline is not UTF-8 text",
        ))
    })
}

/// The text of the timeline file at `path` (see [`TimelineSource::from_path`] for the rules and errors).
fn read_timeline_file(path: &Path) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind};
    let shown = path.display();
    let too_large = |bytes: Option<u64>| {
        let size = match bytes {
            Some(b) => format!("is {:.1} MB", b as f64 / (1024.0 * 1024.0)),
            None => format!("is larger than {} MB", MAX_TIMELINE_BYTES / (1024 * 1024)),
        };
        Error::new(
            ErrorKind::FileTooLarge,
            format!(
                "`{shown}` {size}; a timeline is at most {} MB ({MAX_TIMELINE_BYTES} bytes)",
                MAX_TIMELINE_BYTES / (1024 * 1024)
            ),
        )
    };
    let not_regular = |kind: &str| {
        Error::new(
            ErrorKind::InvalidInput,
            format!(
                "`{shown}` is not a regular file (it is {kind}); a timeline is a YAML or JSON text file"
            ),
        )
    };
    // Look before opening: opening a FIFO for reading blocks until a writer appears.
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Err(Error::new(
            ErrorKind::IsADirectory,
            format!("`{shown}` is a directory, not a timeline file"),
        ));
    }
    if !meta.is_file() {
        return Err(not_regular(file_kind(&meta)));
    }
    if meta.len() > MAX_TIMELINE_BYTES as u64 {
        return Err(too_large(Some(meta.len())));
    }
    let file = std::fs::File::open(path)?;
    // The path may have been replaced between the two calls: check what was opened, too.
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(not_regular(file_kind(&opened)));
    }
    read_bounded(file).map_err(|e| match e {
        BoundedRead::TooLarge => too_large(None),
        BoundedRead::Io(e) if e.kind() == ErrorKind::InvalidData => Error::new(
            ErrorKind::InvalidData,
            format!("`{shown}` is not UTF-8 text"),
        ),
        BoundedRead::Io(e) => e,
    })
}

/// "a FIFO", "a socket", "a device", … for messages about files that are not regular files.
fn file_kind(meta: &std::fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let t = meta.file_type();
        if t.is_fifo() {
            return "a FIFO";
        }
        if t.is_socket() {
            return "a socket";
        }
        if t.is_char_device() || t.is_block_device() {
            return "a device";
        }
    }
    if meta.is_symlink() {
        "a symbolic link"
    } else {
        "not a file"
    }
}

/// Progress notifications emitted during long operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressEvent {
    /// A pipeline stage started. Renders emit, in order: `parse`, `decode` (clip sources), `synthesize`,
    /// `process` (per-cue chain), `layout`, `mix`, `master`, `encode`, `export` (side files).
    Stage {
        /// Stage name.
        stage: String,
        /// Human-readable detail, e.g. "12 cues".
        detail: String,
    },
    /// One cue finished synthesizing (or was served from cache).
    Synthesized {
        /// The cue id.
        cue_id: String,
        /// Served from the TTS cache.
        cached: bool,
        /// Cues finished so far, this one included.
        done: usize,
        /// Cues in the render.
        total: usize,
    },
}

/// Callback receiving [`ProgressEvent`]s. Must be cheap and non-blocking.
pub type ProgressCallback = Arc<dyn Fn(&ProgressEvent) + Send + Sync>;

/// Cooperative cancellation flag for [`Engine::render`]: set it to `true` and the render stops at its next
/// checkpoint (between stages, between synthesis requests and between DSP blocks), deletes the temporary
/// files it wrote and returns a [`RenderFailure`] with `cancelled: true`. Outputs of earlier renders are
/// never touched. Once a render has started to put its files in place (after its last stage) it finishes.
pub type CancelFlag = Arc<std::sync::atomic::AtomicBool>;

/// Engine-wide options.
#[derive(Clone)]
pub struct EngineOptions {
    /// TTS cache directory. None → env `SONICCRAFT_CACHE_DIR` → `<timeline dir>/.soniccraft/cache`.
    pub cache_dir: Option<PathBuf>,
    /// Read cached TTS audio (results are always written to the cache). `--no-cache` sets this to false.
    pub read_cache: bool,
    /// Maximum concurrent synthesis requests overall.
    pub jobs: usize,
    /// ffmpeg binary. None → look up `ffmpeg` on PATH when needed.
    pub ffmpeg_path: Option<PathBuf>,
    /// Receives [`ProgressEvent`]s during renders.
    pub progress: Option<ProgressCallback>,
    /// Allow `type: command` providers (and piper with a custom `command`) to run local programs. The CLI
    /// allows them; the MCP server does not unless it is started with `--allow-command` (or
    /// `SONICCRAFT_ALLOW_COMMAND=1`). When false, cues of such a provider are `provider_unavailable` errors
    /// in validate, plan and render (warnings when every cue is cached, since nothing then runs), and
    /// [`Engine::providers`] reports `command` as unavailable.
    pub allow_command: bool,
    /// Keep output paths that come from the timeline document (`output.path` and the side files and stems
    /// directory derived from it) inside this directory (the MCP server sets it to the base directory).
    /// Paths are resolved through the symlinks of their existing ancestors, so neither `..` nor a symlink
    /// leads out; a violation is an `invalid_value` error at `output.path` in validate, plan and render. A
    /// path given in [`RenderOverrides::output_path`] is exempt (the caller chose it), and so is the
    /// default output next to the timeline file.
    pub confine_outputs_to: Option<PathBuf>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            cache_dir: None,
            read_cache: true,
            jobs: 4,
            ffmpeg_path: None,
            progress: None,
            allow_command: true,
            confine_outputs_to: None,
        }
    }
}

impl std::fmt::Debug for EngineOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineOptions")
            .field("cache_dir", &self.cache_dir)
            .field("read_cache", &self.read_cache)
            .field("jobs", &self.jobs)
            .field("ffmpeg_path", &self.ffmpeg_path)
            .field("progress", &self.progress.is_some())
            .field("allow_command", &self.allow_command)
            .field("confine_outputs_to", &self.confine_outputs_to)
            .finish()
    }
}

/// Options for `validate`.
#[derive(Debug, Clone, Default)]
pub struct ValidateOptions {
    /// Also verify API keys with a cheap authenticated request (network).
    pub check_credentials_online: bool,
}

/// Per-render overrides of the document's `output` block.
#[derive(Debug, Clone, Default)]
pub struct RenderOverrides {
    /// Output path (relative paths resolve against the current directory for the CLI, `base_dir` for MCP).
    pub output_path: Option<PathBuf>,
    /// Output format name (wav, mp3, flac, ogg, opus, m4a, or an alias such as `wave`; case and
    /// surrounding spaces do not matter here). A format that needs a missing ffmpeg is a
    /// `format_requires_ffmpeg` error without a document path: the render option, not the timeline, asked
    /// for it.
    pub format: Option<String>,
    /// Export subtitles (`true`: the document's `export_subtitles` formats, or vtt + srt when it lists
    /// none; `false`: none).
    pub export_subtitles: Option<bool>,
    /// Export one 32-bit float WAV stem per track.
    pub export_stems: Option<bool>,
    /// Write `<out>.manifest.json`.
    pub export_manifest: Option<bool>,
    /// Also write `<out>.preview.html` (web visualizer).
    pub preview: bool,
    /// Cancellation flag (MCP `notifications/cancelled`, CLI Ctrl-C): a cancelled render deletes its
    /// temporary files and leaves every existing output as it was.
    pub cancel: Option<CancelFlag>,
}

/// Errors that are not about the timeline itself (e.g. unknown provider in `voices`).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A bad argument, e.g. an unknown provider name.
    #[error("{0}")]
    InvalidArgument(String),
    /// A provider request failed.
    #[error("{0}")]
    Provider(String),
    /// A filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The engine. Cheap to clone; safe to share across tasks.
#[derive(Clone, Debug)]
pub struct Engine {
    options: EngineOptions,
    /// Where API keys, `SONICCRAFT_CACHE_DIR`, `PATH` and the other provider variables are read from.
    env: EnvSource,
}

impl Engine {
    /// An engine with `options`, reading environment variables from the process environment.
    pub fn new(options: EngineOptions) -> Self {
        Engine {
            options,
            env: EnvSource::Process,
        }
    }

    /// Read API keys, `SONICCRAFT_CACHE_DIR`, `PATH` and the other provider variables from `env` instead of
    /// the process environment (sandboxes, tests, embedders serving several users).
    #[must_use]
    pub fn with_env(mut self, env: EnvSource) -> Self {
        self.env = env;
        self
    }

    /// Replace the options, keeping the environment set with [`Engine::with_env`] (per-call variations
    /// such as a progress callback or `read_cache: false`).
    #[must_use]
    pub fn with_options(mut self, options: EngineOptions) -> Self {
        self.options = options;
        self
    }

    /// The options the engine was created with.
    pub fn options(&self) -> &EngineOptions {
        &self.options
    }

    /// Where the engine reads environment variables from.
    pub fn env(&self) -> &EnvSource {
        &self.env
    }

    /// Parse, lower and check the environment (assets, credentials, the key-sending host policy, local
    /// programs and servers, installed voices, ffmpeg, cache) and the layout (errors that depend on
    /// estimated speech lengths are warnings), plus the render memory budget (a warning). Asset files are
    /// checked even when the document has errors. Never fails: problems are diagnostics.
    pub async fn validate(
        &self,
        source: &TimelineSource,
        options: &ValidateOptions,
    ) -> ValidationReport {
        checks::validate(self, source, options).await
    }

    /// Dry run: validate, then lay out the timeline with cached, mock (rendered in-process, exact) or
    /// estimated durations. Never calls a provider, and needs no memory proportional to the timeline's
    /// length. Never fails: problems are diagnostics.
    pub async fn plan(&self, source: &TimelineSource) -> PlanReport {
        plan::plan(self, source).await
    }

    /// Full render.
    ///
    /// Writes the output file and the requested side files with a two-phase commit: everything goes to
    /// hidden temporary files next to its destination, and all of them are renamed into place only after
    /// the last stage succeeded. On failure or cancellation only those temporary files (and the directories
    /// the render created, when empty) are removed: existing outputs are never deleted or left half
    /// replaced. An existing file in the way that is not an output of its kind (`notes.txt` as the audio
    /// file, a directory as a subtitle file) is an `invalid_value` error before anything runs.
    ///
    /// Renders stop before any TTS request when the timeline cannot be laid out whatever the exact speech
    /// lengths turn out to be, or when it would need more memory than the render budget (8 GB, or
    /// `SONICCRAFT_MAX_RENDER_GB` in the engine's environment; `timeline_too_long`). The returned report has
    /// absolute paths and stage timings.
    pub async fn render(
        &self,
        source: &TimelineSource,
        overrides: &RenderOverrides,
    ) -> Result<RenderReport, RenderFailure> {
        render::render(self, source, overrides).await
    }

    /// Built-in providers and their availability in this environment: credentials, binaries, platform,
    /// the `allow_command` policy, and whether local servers (Kokoro-FastAPI on `localhost`, ...) answer a
    /// quick TCP probe (at most 300 ms, once per distinct URL; this call blocks for that long at most).
    pub fn providers(&self) -> ProvidersReport {
        providers::report(self)
    }

    /// Voices offered by a provider (built-in type name or alias). May call the provider's API.
    pub async fn voices(&self, provider: &str) -> Result<VoicesReport, EngineError> {
        providers::voices(self, provider).await
    }

    /// Resolve the TTS cache directory for an optional timeline source (DESIGN.md §5.1):
    /// [`EngineOptions::cache_dir`] > env `SONICCRAFT_CACHE_DIR` > `<timeline dir>/.soniccraft/cache` >
    /// `<current dir>/.soniccraft/cache` when there is no source. Always absolute.
    pub fn cache_dir(&self, source: Option<&TimelineSource>) -> PathBuf {
        cache::dir(self, source)
    }

    /// Cache statistics.
    pub fn cache_info(&self, source: Option<&TimelineSource>) -> std::io::Result<CacheInfo> {
        cache::info(self, source)
    }

    /// Delete all cache entries; returns how many were removed.
    pub fn cache_clear(&self, source: Option<&TimelineSource>) -> std::io::Result<usize> {
        cache::clear(self, source)
    }
}

/// The timeline JSON Schema.
pub fn timeline_schema() -> serde_json::Value {
    soniccraft_core::schema::timeline_schema()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn engine_futures_are_send() {
        let engine = Engine::new(EngineOptions::default());
        let source = TimelineSource::from_text("version: \"1.0\"", ".", SourceFormat::Yaml);
        let overrides = RenderOverrides::default();
        let validate_options = ValidateOptions::default();
        assert_send(&engine.validate(&source, &validate_options));
        assert_send(&engine.plan(&source));
        assert_send(&engine.render(&source, &overrides));
        assert_send(&engine.voices("mock"));
    }

    #[test]
    fn with_options_keeps_the_environment() {
        let engine = Engine::new(EngineOptions::default())
            .with_env(EnvSource::fixed([("SONICCRAFT_TEST_VAR", "kept")]));
        let options = EngineOptions {
            read_cache: false,
            ..EngineOptions::default()
        };
        let copy = engine.clone().with_options(options);
        assert!(!copy.options().read_cache);
        assert_eq!(
            copy.env().get("SONICCRAFT_TEST_VAR").as_deref(),
            Some("kept")
        );
    }

    #[test]
    fn engine_is_shareable() {
        fn assert_sync<T: Send + Sync + Clone>() {}
        assert_sync::<Engine>();
    }
}
