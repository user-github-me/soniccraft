//! `soniccraft render <TIMELINE> [-o PATH] [--format F] [--subtitles] [--stems] [--manifest] [--preview]
//! [--no-cache] [--json]`
//!
//! Ctrl-C sets the engine's cancellation flag; the engine stops at its next checkpoint, removes partial
//! outputs and returns a cancelled `RenderFailure` (exit 130). A second Ctrl-C exits immediately.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::codes;
use soniccraft_engine::{
    CancelFlag, Engine, RenderFailure, RenderOverrides, RenderReport, TimelineSource,
};

use super::kv;
use crate::cli::RenderArgs;
use crate::context::{Ctx, Loaded};
use crate::exit::{CliError, CmdResult, Exit};
use crate::format::{bytes, db, duration_ms, plural};
use crate::progress::Progress;
use crate::term::Style;

/// Error codes of a render that failed while running (exit 3); any other error code means the timeline
/// is invalid in this environment (exit 1).
const RUNTIME_ERROR_CODES: [&str; 3] = [
    codes::SYNTHESIS_FAILED,
    codes::ENCODE_FAILED,
    codes::DECODE_FAILED,
];

/// Error codes that report the environment rather than the document: a key or a server missing here, a
/// voice not installed here, more audio than this machine renders (and see [`option_caused`]). They still
/// exit 1 (the timeline cannot be rendered as things are, DESIGN §9.1), but the summary does not blame
/// the timeline.
const ENVIRONMENT_ERROR_CODES: [&str; 4] = [
    codes::MISSING_CREDENTIALS,
    codes::PROVIDER_UNAVAILABLE,
    codes::UNKNOWN_VOICE,
    codes::TIMELINE_TOO_LONG,
];

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &RenderArgs) -> CmdResult {
    let loaded = ctx.load(&args.timeline)?;
    let output_path = args
        .output
        .as_deref()
        .map(|p| output_file(ctx, p))
        .transpose()?;

    let progress = (!ctx.quiet && ctx.err.is_terminal())
        .then(|| Progress::new(ctx.err.color(), ctx.verbose == 0, &ctx.cwd));
    let mut options = ctx.engine_options(progress.as_ref().map(Progress::callback));
    options.read_cache = !args.no_cache;
    let engine = Engine::new(options);

    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    #[allow(clippy::needless_update)] // the engine may add override fields
    let overrides = RenderOverrides {
        output_path,
        format: args.format.map(|f| f.as_str().to_string()),
        export_subtitles: args.subtitles.then_some(true),
        export_stems: args.stems.then_some(true),
        export_manifest: args.manifest.then_some(true),
        preview: args.preview,
        cancel: Some(Arc::clone(&cancel)),
        ..RenderOverrides::default()
    };

    let runtime = ctx.runtime()?;
    let started = Instant::now();
    let (result, interrupted) = runtime.block_on(render_until_interrupted(
        &engine,
        &loaded.source,
        &overrides,
        &cancel,
        progress.as_deref(),
    ));
    let elapsed = started.elapsed();
    if let Some(p) = &progress {
        p.finish();
    }
    // Do not wait for stray background work (e.g. a provider request the engine abandoned).
    runtime.shutdown_timeout(Duration::from_millis(500));

    match result {
        Ok(report) => {
            if ctx.json {
                ctx.print_json(&report)?;
            } else {
                print_report(ctx, &loaded, &report, elapsed);
            }
            if interrupted && !ctx.quiet {
                ctx.warn("the render finished before it could be cancelled");
            }
            Ok(Exit::Success)
        }
        Err(failure) => report_failure(ctx, &loaded, &failure, args.format.is_some()),
    }
}

/// Resolve `-o` against the current directory and reject directories.
fn output_file(ctx: &Ctx, path: &Path) -> Result<PathBuf, CliError> {
    let raw = path.as_os_str().to_string_lossy();
    let absolute = ctx.absolute(path);
    if raw.ends_with('/') || raw.ends_with(std::path::MAIN_SEPARATOR) || absolute.is_dir() {
        return Err(CliError::usage(format!(
            "`-o {}` is a directory; -o takes the output file",
            path.display()
        ))
        .with_hint(format!(
            "name the file, e.g. `-o {}`",
            Path::new(&*raw).join("episode.wav").display()
        )));
    }
    Ok(absolute)
}

/// Run the render; the first Ctrl-C requests cancellation, a second one exits the process.
/// Returns the engine's result and whether Ctrl-C was pressed.
async fn render_until_interrupted(
    engine: &Engine,
    source: &TimelineSource,
    overrides: &RenderOverrides,
    cancel: &CancelFlag,
    progress: Option<&Progress>,
) -> (Result<RenderReport, RenderFailure>, bool) {
    let render = engine.render(source, overrides);
    tokio::pin!(render);
    let signal = tokio::select! {
        result = &mut render => return (result, false),
        signal = tokio::signal::ctrl_c() => signal,
    };
    if signal.is_err() {
        // No signal handler could be installed: just wait for the render.
        return (render.await, false);
    }
    cancel.store(true, Ordering::SeqCst);
    let message = "cancelling the render (press Ctrl-C again to quit immediately)";
    match progress {
        Some(p) => p.notice(message),
        None => eprint_line(message),
    }
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprint_line("interrupted");
            // `exit` (not `_exit` or `abort`): it runs the `atexit` handler of soniccraft-tts, which kills
            // the process groups of running TTS programs (piper, `command` providers). Those no longer
            // receive the terminal's Ctrl-C themselves.
            std::process::exit(i32::from(Exit::Interrupted.code()));
        }
    });
    (render.await, true)
}

fn eprint_line(message: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{message}");
}

/// The exit status of a failed render.
fn failure_exit(cancelled: bool, diagnostics: &[Diagnostic]) -> Exit {
    if cancelled {
        return Exit::Interrupted;
    }
    let invalid = diagnostics
        .iter()
        .any(|d| d.is_error() && !RUNTIME_ERROR_CODES.contains(&d.code.as_str()));
    if invalid {
        Exit::Invalid
    } else {
        Exit::Failure
    }
}

/// Whether `d` is about a render option rather than the document: `format_requires_ffmpeg` for a format
/// that `--format` (`format_option`) or the extension of `-o` chose. The engine gives those no document
/// path; they are shown without a source snippet.
fn option_caused(d: &Diagnostic, format_option: bool) -> bool {
    d.code == codes::FORMAT_REQUIRES_FFMPEG
        && (format_option || (d.path.is_none() && d.line.is_none()))
}

/// The `error:` line that ends a failed render's output: the engine's message, except that a claim
/// "the timeline has N errors" becomes `render stopped: N errors` when every error is an environment or
/// option problem (the timeline itself is fine). `format_option`: `--format` was given.
fn failure_summary(failure: &RenderFailure, format_option: bool) -> String {
    let message = failure.message.trim();
    let errors: Vec<&Diagnostic> = failure
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .collect();
    let environmental = |d: &Diagnostic| {
        ENVIRONMENT_ERROR_CODES.contains(&d.code.as_str()) || option_caused(d, format_option)
    };
    let blames_the_timeline = message.is_empty() || message.starts_with("the timeline has");
    if blames_the_timeline && !errors.is_empty() && errors.iter().all(|d| environmental(d)) {
        return format!(
            "render stopped: {}",
            plural(errors.len(), "error", "errors")
        );
    }
    if message.is_empty() {
        "render failed".to_string()
    } else {
        message.to_string()
    }
}

fn report_failure(
    ctx: &Ctx,
    loaded: &Loaded,
    failure: &RenderFailure,
    format_option: bool,
) -> CmdResult {
    let exit = failure_exit(failure.cancelled, &failure.diagnostics);
    if ctx.json {
        ctx.print_json(failure)?;
    }
    if failure.cancelled {
        ctx.err.line("render cancelled");
        return Ok(exit);
    }
    if !ctx.json {
        let view = loaded.view();
        ctx.print_diagnostics_with(&ctx.err, &failure.diagnostics, Some(&view), &|d| {
            !option_caused(d, format_option)
        });
    }
    ctx.error(&failure_summary(failure, format_option), None);
    Ok(exit)
}

/// Diagnostics on stderr, the result block on stdout (`-q`: just the output path).
fn print_report(ctx: &Ctx, loaded: &Loaded, report: &RenderReport, elapsed: Duration) {
    let view = loaded.view();
    let shown = ctx.print_diagnostics(&ctx.err, &report.diagnostics, Some(&view));
    let output = Path::new(&report.output.path);
    if ctx.quiet {
        ctx.out.line(&ctx.show(output));
        return;
    }
    let term = &ctx.out;
    let out_dir = output.parent().unwrap_or(Path::new(""));
    // Side files next to the output are shown by name, others by path.
    let side = |path: &str| {
        let p = Path::new(path);
        if p.parent() == Some(out_dir) {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| ctx.show(p))
        } else {
            ctx.show(p)
        }
    };

    let mut text = format!(
        "{} {} in {}\n",
        term.paint(Style::Success, "Rendered"),
        term.paint(Style::Bold, &ctx.show(output)),
        duration_ms(elapsed.as_secs_f64() * 1000.0)
    );
    let o = &report.output;
    let channels = match o.channels {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n} channels"),
    };
    let mut format_parts = vec![o.format.clone(), format!("{} Hz", o.sample_rate), channels];
    if let Some(bits) = o.bit_depth {
        format_parts.push(format!("{bits}-bit"));
    }
    if let Some(bps) = o.bitrate_bps {
        format_parts.push(format!("{} kbps", bps / 1000));
    }
    format_parts.push(bytes(o.bytes));
    text.push_str(&kv(term, "format", &format_parts.join(", ")));
    text.push_str(&kv(term, "duration", &duration_ms(o.duration_ms)));
    text.push_str(&kv(term, "loudness", &loudness_line(ctx, report)));

    let f = &report.files;
    if !f.subtitles.is_empty() {
        let names: Vec<String> = f.subtitles.iter().map(|s| side(&s.path)).collect();
        text.push_str(&kv(term, "subtitles", &names.join(", ")));
    }
    if let Some(first) = f.stems.first() {
        let dir = Path::new(&first.path)
            .parent()
            .map(|d| side(&d.to_string_lossy()))
            .unwrap_or_default();
        text.push_str(&kv(
            term,
            "stems",
            &format!("{} in {dir}/", plural(f.stems.len(), "file", "files")),
        ));
    }
    if let Some(manifest) = &f.manifest {
        text.push_str(&kv(term, "manifest", &side(manifest)));
    }
    if let Some(preview) = &f.preview {
        text.push_str(&kv(term, "preview", &side(preview)));
    }

    let s = &report.synthesis;
    let mut synthesis = format!(
        "{}: {} synthesized, {} from cache",
        plural(s.cues, "cue", "cues"),
        s.synthesized,
        s.cache_hits
    );
    if !s.providers_called.is_empty() {
        synthesis.push_str(&format!(" (called {})", s.providers_called.join(", ")));
    }
    text.push_str(&kv(term, "synthesis", &synthesis));

    if shown.errors + shown.warnings + shown.infos + shown.hidden_infos > 0 {
        let mut diagnostics = if shown.errors + shown.warnings + shown.infos > 0 {
            format!("{} (on stderr)", shown.counts())
        } else {
            String::new()
        };
        if shown.hidden_infos > 0 {
            if !diagnostics.is_empty() {
                diagnostics.push_str("; ");
            }
            diagnostics.push_str(&format!(
                "{} hidden, use -v to show",
                plural(shown.hidden_infos, "note", "notes")
            ));
        }
        text.push_str(&kv(term, "diagnostics", &diagnostics));
    }
    term.write(&text);
}

/// `-16.0 LUFS (target -16.0), true peak -1.4 dBTP (ceiling -1.0), LRA 4.2 LU`, with misses highlighted.
fn loudness_line(ctx: &Ctx, report: &RenderReport) -> String {
    let l = &report.loudness;
    let term = &ctx.out;
    let integrated = match l.integrated_lufs {
        Some(i) => {
            let text = format!("{} LUFS", db(i));
            if l.normalized && (i - l.target_lufs).abs() > 0.5 {
                term.paint(Style::Warning, &text)
            } else {
                text
            }
        }
        None => "n/a (silent or too short)".to_string(),
    };
    let target = if l.normalized {
        format!("(target {})", db(l.target_lufs))
    } else {
        "(not normalized)".to_string()
    };
    let mut line = format!("{integrated} {target}");
    if let Some(tp) = l.true_peak_dbtp {
        let text = format!("true peak {} dBTP", db(tp));
        let text = if tp > l.true_peak_target_dbtp + 0.1 {
            term.paint(Style::Warning, &text)
        } else {
            text
        };
        line.push_str(&format!(
            ", {text} (ceiling {})",
            db(l.true_peak_target_dbtp)
        ));
    }
    if let Some(lra) = l.loudness_range_lu {
        line.push_str(&format!(", LRA {} LU", db(lra)));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostics(codes: &[&str]) -> Vec<Diagnostic> {
        codes
            .iter()
            .map(|c| Diagnostic::error(c, "x"))
            .chain([Diagnostic::warning("silent_output", "w")])
            .collect()
    }

    fn failure(codes: &[&str]) -> RenderFailure {
        RenderFailure {
            message: "the timeline has 9 errors; fix them and render again".to_string(),
            diagnostics: diagnostics(codes),
            cancelled: false,
        }
    }

    #[test]
    fn environment_failures_do_not_blame_the_timeline() {
        let summary =
            |codes: &[&str], format_option| failure_summary(&failure(codes), format_option);
        assert_eq!(
            summary(&["missing_credentials"], false),
            "render stopped: 1 error"
        );
        assert_eq!(
            summary(
                &["provider_unavailable", "unknown_voice", "timeline_too_long"],
                false
            ),
            "render stopped: 3 errors"
        );
        // `format_requires_ffmpeg` is an option problem when `--format` asked for the format, or when the
        // engine says so by leaving the document out (the extension of `-o` chose it).
        assert_eq!(
            summary(&["format_requires_ffmpeg"], true),
            "render stopped: 1 error"
        );
        assert_eq!(
            summary(&["format_requires_ffmpeg"], false),
            "render stopped: 1 error"
        );
        let mut document = failure(&[]);
        document.diagnostics = vec![
            Diagnostic::error("format_requires_ffmpeg", "x")
                .with_path("output.format")
                .at(3, 11),
        ];
        assert_eq!(
            failure_summary(&document, false),
            "the timeline has 9 errors; fix them and render again"
        );
        assert_eq!(failure_summary(&document, true), "render stopped: 1 error");
        // A message that does not blame the timeline is kept.
        let mut worded = failure(&["missing_credentials"]);
        worded.message = "render stopped: 1 error; fix it and render again".to_string();
        assert_eq!(
            failure_summary(&worded, false),
            "render stopped: 1 error; fix it and render again"
        );
        // Any document error keeps the engine's message, and so do runtime failures.
        assert_eq!(
            summary(&["missing_credentials", "unknown_reference"], true),
            "the timeline has 9 errors; fix them and render again"
        );
        assert_eq!(
            summary(&["synthesis_failed"], false),
            "the timeline has 9 errors; fix them and render again"
        );
        assert_eq!(
            summary(&[], false),
            "the timeline has 9 errors; fix them and render again"
        );
        let mut empty = failure(&["unknown_reference"]);
        empty.message = " ".to_string();
        assert_eq!(failure_summary(&empty, false), "render failed");
        empty.diagnostics = diagnostics(&["provider_unavailable"]);
        assert_eq!(failure_summary(&empty, false), "render stopped: 1 error");
        // Exit statuses are unchanged (DESIGN §9.1).
        assert_eq!(
            failure_exit(false, &diagnostics(&["missing_credentials"])),
            Exit::Invalid
        );
    }

    #[test]
    fn failure_classification() {
        let exit = |codes: &[&str], cancelled| failure_exit(cancelled, &diagnostics(codes));
        assert_eq!(exit(&["synthesis_failed"], false), Exit::Failure);
        assert_eq!(exit(&[], false), Exit::Failure);
        assert_eq!(exit(&["unknown_reference"], false), Exit::Invalid);
        assert_eq!(
            exit(&["encode_failed", "asset_not_found"], false),
            Exit::Invalid
        );
        assert_eq!(exit(&["unknown_reference"], true), Exit::Interrupted);
    }
}
