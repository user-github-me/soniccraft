//! What every command needs: the output streams, verbosity, the current directory, engine construction,
//! timeline loading and the shared report/error printing.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use soniccraft_core::diagnostics::codes;
use soniccraft_core::parse::MAX_DOCUMENT_BYTES;
use soniccraft_core::{Diagnostic, SourceFormat};
use soniccraft_engine::{Engine, EngineOptions, ProgressCallback, TimelineSource};

use crate::cli::GlobalArgs;
use crate::diag::{self, Filter, Rendered, SourceView};
use crate::exit::{CliError, Exit};
use crate::format;
use crate::term::{Stream, Style, Term};

/// Shared state of one CLI invocation.
#[derive(Debug)]
pub(crate) struct Ctx {
    /// stdout: reports, results.
    pub(crate) out: Term,
    /// stderr: progress, logs, errors.
    pub(crate) err: Term,
    /// `-v` count.
    pub(crate) verbose: u8,
    /// `-q`.
    pub(crate) quiet: bool,
    /// The command was asked for `--json`.
    pub(crate) json: bool,
    /// The current directory (relative paths given on the command line resolve against it).
    pub(crate) cwd: PathBuf,
    /// `--cache-dir`, made absolute.
    pub(crate) cache_dir: Option<PathBuf>,
    /// `--jobs`.
    pub(crate) jobs: Option<usize>,
}

/// A timeline read from a file or stdin.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    /// The engine input.
    pub(crate) source: TimelineSource,
    /// The name diagnostics show (`examples/demo.yaml` as typed, or `<stdin>`).
    pub(crate) name: String,
}

impl Loaded {
    /// A view of the text for rustc-style snippets.
    pub(crate) fn view(&self) -> SourceView<'_> {
        SourceView::new(&self.name, &self.source.text, self.source.format)
    }
}

impl Ctx {
    /// Build the context from the global options.
    pub(crate) fn new(global: &GlobalArgs, json: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let cache_dir = global
            .cache_dir
            .as_deref()
            .map(|dir| format::absolute(dir, &cwd));
        Ctx {
            out: Term::new(Stream::Stdout, global.color),
            err: Term::new(Stream::Stderr, global.color),
            verbose: global.verbose,
            quiet: global.quiet,
            json,
            cwd,
            cache_dir,
            jobs: global.jobs.map(usize::from),
        }
    }

    /// Engine options from the global flags.
    pub(crate) fn engine_options(&self, progress: Option<ProgressCallback>) -> EngineOptions {
        let defaults = EngineOptions::default();
        EngineOptions {
            cache_dir: self.cache_dir.clone(),
            jobs: self.jobs.unwrap_or(defaults.jobs),
            progress,
            ..defaults
        }
    }

    /// An engine configured from the global flags.
    pub(crate) fn engine(&self) -> Engine {
        Engine::new(self.engine_options(None))
    }

    /// A multi-threaded tokio runtime for the async engine operations.
    pub(crate) fn runtime(&self) -> Result<tokio::runtime::Runtime, CliError> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("soniccraft")
            .build()
            .map_err(|e| CliError::failure(format!("cannot start the async runtime: {e}")))
    }

    /// `path` made absolute against the current directory.
    pub(crate) fn absolute(&self, path: &Path) -> PathBuf {
        format::absolute(path, &self.cwd)
    }

    /// `path` for display: relative to the current directory when inside it.
    pub(crate) fn show(&self, path: &Path) -> String {
        format::show_path(path, &self.cwd)
    }

    /// `path` given as a string in a report (absolute) for display.
    pub(crate) fn show_str(&self, path: &str) -> String {
        self.show(Path::new(path))
    }

    /// Read the timeline at `path` (`-` = stdin). At most [`MAX_DOCUMENT_BYTES`] are accepted, and at most
    /// one byte more is ever read, so `/dev/zero`, a FIFO or a huge file fail fast. Any readable file that
    /// is not a directory works (`<(generate-timeline)` too).
    pub(crate) fn load(&self, path: &Path) -> Result<Loaded, CliError> {
        if path.as_os_str() == "-" {
            let bytes = read_limited(std::io::stdin().lock(), MAX_DOCUMENT_BYTES).map_err(|e| {
                CliError::failure(format!("cannot read the timeline from stdin: {e}"))
            })?;
            let text = decode("the timeline on stdin", SourceFormat::Auto, None, bytes)?;
            return Ok(Loaded {
                source: TimelineSource::from_text(text, self.cwd.clone(), SourceFormat::Auto),
                name: "<stdin>".to_string(),
            });
        }
        let shown = path.display().to_string();
        let what = format!("timeline file `{shown}`");
        let absolute = self.absolute(path);
        let format = SourceFormat::from_extension(absolute.extension().and_then(|e| e.to_str()));
        let unreadable = |e: std::io::Error| match e.kind() {
            std::io::ErrorKind::NotFound => CliError::failure(format!("{what} not found"))
                .with_hint("check the path, or create a starter timeline with `soniccraft init`"),
            _ => CliError::failure(format!("cannot read {what}: {e}")),
        };
        let metadata = std::fs::metadata(&absolute).map_err(unreadable)?;
        if metadata.is_dir() {
            return Err(CliError::failure(format!(
                "`{shown}` is a directory, not a timeline file"
            ))
            .with_hint("pass the timeline file, e.g. `timeline.yaml` inside it"));
        }
        let size = metadata.is_file().then_some(metadata.len());
        if size.is_some_and(|n| n > MAX_DOCUMENT_BYTES as u64) {
            return Err(too_large(&what, size, format, ""));
        }
        let file = std::fs::File::open(&absolute).map_err(unreadable)?;
        let bytes = read_limited(file, MAX_DOCUMENT_BYTES).map_err(unreadable)?;
        let text = decode(&what, format, size, bytes)?;
        // As `TimelineSource::from_path`: relative paths in the document resolve against its directory.
        let base_dir = absolute
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut source = TimelineSource::from_text(text, base_dir, format);
        source.path = Some(absolute);
        Ok(Loaded {
            source,
            name: shown,
        })
    }

    /// Print `value` as pretty JSON (one document plus a newline) on stdout.
    pub(crate) fn print_json<T: Serialize>(&self, value: &T) -> Result<(), CliError> {
        let mut text = serde_json::to_string_pretty(value)
            .map_err(|e| CliError::failure(format!("cannot serialize the report: {e}")))?;
        text.push('\n');
        self.out.write(&text);
        Ok(())
    }

    /// The severity filter for human output.
    pub(crate) fn filter(&self) -> Filter {
        Filter {
            warnings: !self.quiet,
            infos: self.verbose > 0,
            paths: self.verbose > 0,
        }
    }

    /// Render `diagnostics` (rustc style) to `term`.
    pub(crate) fn print_diagnostics(
        &self,
        term: &Term,
        diagnostics: &[Diagnostic],
        view: Option<&SourceView<'_>>,
    ) -> Rendered {
        let rendered = diag::render_all(diagnostics, view, term.color(), self.filter());
        term.write(&rendered.text);
        rendered
    }

    /// [`Ctx::print_diagnostics`], showing the diagnostics for which `in_document` is false without
    /// source snippets (see [`diag::render_all_with`]).
    pub(crate) fn print_diagnostics_with(
        &self,
        term: &Term,
        diagnostics: &[Diagnostic],
        view: Option<&SourceView<'_>>,
        in_document: &dyn Fn(&Diagnostic) -> bool,
    ) -> Rendered {
        let rendered =
            diag::render_all_with(diagnostics, view, term.color(), self.filter(), in_document);
        term.write(&rendered.text);
        rendered
    }

    /// Report a command failure: `error: …` (+ hint) on stderr, and the failure document on stdout with
    /// `--json`. Returns the failure's exit status.
    pub(crate) fn report_failure(&self, error: &CliError) -> Exit {
        if self.json {
            let doc = FailureDoc {
                message: &error.message,
                diagnostics: &error.diagnostics,
                hint: error.hint.as_deref(),
            };
            // Serializing strings and diagnostics cannot fail; ignore the impossible error rather than
            // recurse.
            let _ = self.print_json(&doc);
        }
        self.error(&error.message, error.hint.as_deref());
        error.exit
    }

    /// `error: message` (+ `= hint:`) on stderr.
    pub(crate) fn error(&self, message: &str, hint: Option<&str>) {
        let mut text = format!(
            "{}{}\n",
            self.err.paint(Style::Error, "error"),
            self.err.paint(Style::Bold, &format!(": {message}"))
        );
        if let Some(hint) = hint {
            text.push_str(&format!(
                "  {} {}: {hint}\n",
                self.err.paint(Style::Gutter, "="),
                self.err.paint(Style::Bold, "hint")
            ));
        }
        self.err.write(&text);
    }

    /// `warning: message` on stderr (suppressed by `-q`).
    pub(crate) fn warn(&self, message: &str) {
        if self.quiet {
            return;
        }
        self.err.line(&format!(
            "{}{}",
            self.err.paint(Style::Warning, "warning"),
            self.err.paint(Style::Bold, &format!(": {message}"))
        ));
    }

    /// A human status line on `term`: `<label>: <message>` with a colored label.
    pub(crate) fn status(&self, term: &Term, style: Style, label: &str, message: &str) {
        term.line(&format!("{}: {message}", term.paint(style, label)));
    }
}

/// The JSON document printed on stdout when a command fails before producing its report (`--json`).
/// Its shape matches `RenderFailure` (plus an optional hint), so agents parse one failure shape.
#[derive(Debug, Serialize)]
pub(crate) struct FailureDoc<'a> {
    pub(crate) message: &'a str,
    pub(crate) diagnostics: &'a [Diagnostic],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hint: Option<&'a str>,
}

/// What [`read_limited`] read: everything, or the first `limit + 1` bytes of something larger.
enum Limited {
    Within(Vec<u8>),
    Over(Vec<u8>),
}

/// Read `reader` to the end, but never more than `limit + 1` bytes.
fn read_limited(reader: impl Read, limit: usize) -> std::io::Result<Limited> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(if bytes.len() > limit {
        Limited::Over(bytes)
    } else {
        Limited::Within(bytes)
    })
}

/// The text of a timeline that [`read_limited`] read, or why it cannot be used. `what` names the input
/// in messages (``timeline file `x` ``); `size` is the file size when the input is a regular file.
fn decode(
    what: &str,
    format: SourceFormat,
    size: Option<u64>,
    bytes: Limited,
) -> Result<String, CliError> {
    match bytes {
        Limited::Within(bytes) => String::from_utf8(bytes).map_err(|_| {
            CliError::failure(format!("{what} is not UTF-8 text"))
                .with_hint("save the file as UTF-8 (YAML and JSON timelines are text files)")
        }),
        Limited::Over(bytes) => {
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
            Err(too_large(what, size, format, &head))
        }
    }
}

/// The failure for a document over [`MAX_DOCUMENT_BYTES`]: exit 1 (the timeline is unusable as it is),
/// with the size error core reports for such documents (`yaml_syntax`/`json_syntax` at 1:1) in the
/// `--json` document. `head` is the start of the text, for telling JSON from YAML.
fn too_large(what: &str, size: Option<u64>, format: SourceFormat, head: &str) -> CliError {
    const MIB: f64 = 1024.0 * 1024.0;
    let max = MAX_DOCUMENT_BYTES / (1024 * 1024);
    // A size that would print as the maximum itself ("5.0 MB") reads better as "larger than".
    let size = size
        .map(|n| n as f64 / MIB)
        .filter(|&mb| mb >= max as f64 + 0.05)
        .map(|mb| format!("{mb:.1} MB"));
    let (message, detail) = match &size {
        Some(size) => (
            format!("{what} is {size}; the maximum is {max} MB"),
            format!("the document is {size}; the maximum is {max} MB"),
        ),
        None => (
            format!("{what} is larger than the {max} MB maximum"),
            format!("the document is larger than the {max} MB maximum"),
        ),
    };
    let hint = "Split the composition into several timelines, or shorten very long texts";
    let code = if format.resolve(head) == SourceFormat::Json {
        codes::JSON_SYNTAX
    } else {
        codes::YAML_SYNTAX
    };
    CliError {
        exit: Exit::Invalid,
        message,
        hint: Some(hint.to_string()),
        diagnostics: vec![Diagnostic::error(code, detail).with_hint(hint).at(1, 1)],
    }
}
