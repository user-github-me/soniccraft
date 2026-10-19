//! `soniccraft validate <TIMELINE> [--check-credentials] [--json]`

use soniccraft_engine::{TimelineSummary, ValidateOptions};

use super::kv;
use crate::cli::ValidateArgs;
use crate::context::Ctx;
use crate::exit::{CmdResult, Exit};
use crate::format::{db, plural};
use crate::term::Style;

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &ValidateArgs) -> CmdResult {
    let loaded = ctx.load(&args.timeline)?;
    let engine = ctx.engine();
    #[allow(clippy::needless_update)] // the engine may add options
    let options = ValidateOptions {
        check_credentials_online: args.check_credentials,
        ..ValidateOptions::default()
    };
    let runtime = ctx.runtime()?;
    let report = runtime.block_on(engine.validate(&loaded.source, &options));
    let exit = Exit::from_diagnostics(report.valid, &report.diagnostics);

    if ctx.json {
        ctx.print_json(&report)?;
        return Ok(exit);
    }

    let view = loaded.view();
    let shown = ctx.print_diagnostics(&ctx.out, &report.diagnostics, Some(&view));
    if exit == Exit::Invalid {
        ctx.status(
            &ctx.out,
            Style::Error,
            "error",
            &format!(
                "{} is invalid: {}{}",
                loaded.name,
                shown.counts(),
                shown.hidden_note()
            ),
        );
        return Ok(exit);
    }
    if ctx.quiet {
        return Ok(exit);
    }
    let problems = shown.errors + shown.warnings + shown.infos;
    let counts = if problems > 0 {
        format!(" ({})", shown.counts())
    } else {
        String::new()
    };
    ctx.status(
        &ctx.out,
        Style::Success,
        "ok",
        &format!("{} is valid{counts}{}", loaded.name, shown.hidden_note()),
    );
    if let Some(summary) = &report.summary {
        ctx.out.write(&summary_block(ctx, summary));
    }
    Ok(exit)
}

/// The facts shown under a valid timeline.
fn summary_block(ctx: &Ctx, s: &TimelineSummary) -> String {
    let term = &ctx.out;
    let list = |items: &[String]| {
        if items.is_empty() {
            "-".to_string()
        } else {
            items.join(", ")
        }
    };
    let mut out = String::new();
    out.push_str(&kv(
        term,
        "tracks",
        &format!(
            "{} ({}, {})",
            s.tracks,
            plural(s.speech_cues, "speech cue", "speech cues"),
            plural(s.clips, "clip", "clips")
        ),
    ));
    out.push_str(&kv(term, "speakers", &list(&s.speakers)));
    out.push_str(&kv(term, "providers", &list(&s.providers)));
    out.push_str(&kv(
        term,
        "speech",
        &format!(
            "{}, {} of {} cached",
            plural(s.characters, "character", "characters"),
            s.cached_cues,
            plural(s.speech_cues, "cue", "cues")
        ),
    ));
    out.push_str(&kv(
        term,
        "output",
        &format!(
            "{}, {} Hz, target {} LUFS",
            s.output_format,
            s.sample_rate,
            db(s.target_lufs)
        ),
    ));
    out
}
