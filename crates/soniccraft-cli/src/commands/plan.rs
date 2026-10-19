//! `soniccraft plan <TIMELINE> [--json]` (alias `dry-run`)

use soniccraft_engine::PlanReport;

use crate::cli::PlanArgs;
use crate::context::Ctx;
use crate::exit::{CmdResult, Exit};
use crate::format::{Align, db, duration_ms, plural, table, timestamp_ms};
use crate::term::Style;

/// Ducking regions listed per track before `…`.
const MAX_REGIONS: usize = 4;

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &PlanArgs) -> CmdResult {
    let loaded = ctx.load(&args.timeline)?;
    let engine = ctx.engine();
    let runtime = ctx.runtime()?;
    let report = runtime.block_on(engine.plan(&loaded.source));
    let exit = Exit::from_diagnostics(report.valid, &report.diagnostics);

    if ctx.json {
        ctx.print_json(&report)?;
        return Ok(exit);
    }

    let view = loaded.view();
    let shown = ctx.print_diagnostics(&ctx.out, &report.diagnostics, Some(&view));
    if !report.diagram.trim().is_empty() {
        let mut diagram = report.diagram.clone();
        if !diagram.ends_with('\n') {
            diagram.push('\n');
        }
        ctx.out.write(&diagram);
        ctx.out.write("\n");
    }
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
    ctx.out.write(&synthesis_section(ctx, &report));
    ctx.out.write(&ducking_section(ctx, &report));
    let problems = shown.errors + shown.warnings + shown.infos;
    let counts = if problems > 0 {
        format!(", {}", shown.counts())
    } else {
        String::new()
    };
    let estimated = if report.estimated {
        ", durations partly estimated"
    } else {
        ""
    };
    ctx.status(
        &ctx.out,
        Style::Success,
        "ok",
        &format!(
            "{}: {} planned{estimated}{counts}{}",
            loaded.name,
            duration_ms(report.duration_ms),
            shown.hidden_note()
        ),
    );
    Ok(exit)
}

/// What a render would synthesize, per provider instance.
fn synthesis_section(ctx: &Ctx, report: &PlanReport) -> String {
    let s = &report.synthesis;
    let mut out = format!(
        "{} {}, {} cached, {} to synthesize\n",
        ctx.out.paint(Style::Bold, "synthesis:"),
        plural(s.cues, "cue", "cues"),
        s.cached,
        plural(s.characters_to_synthesize, "character", "characters")
    );
    if !s.by_provider.is_empty() {
        let rows: Vec<Vec<String>> = s
            .by_provider
            .iter()
            .map(|(name, p)| {
                vec![
                    name.clone(),
                    p.provider_type.clone(),
                    p.cues.to_string(),
                    p.cached.to_string(),
                    p.characters.to_string(),
                ]
            })
            .collect();
        let t = table(
            ctx.out.color(),
            &[
                ("PROVIDER", Align::Left),
                ("TYPE", Align::Left),
                ("CUES", Align::Right),
                ("CACHED", Align::Right),
                ("CHARACTERS", Align::Right),
            ],
            &rows,
        );
        for line in t.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    out
}

/// Ducking regions per ducked track.
fn ducking_section(ctx: &Ctx, report: &PlanReport) -> String {
    if report.ducking.is_empty() {
        return String::new();
    }
    let mut out = format!("{}\n", ctx.out.paint(Style::Bold, "ducking:"));
    let rows: Vec<Vec<String>> = report
        .ducking
        .iter()
        .map(|d| {
            let mut regions: Vec<String> = d
                .regions
                .iter()
                .take(MAX_REGIONS)
                .map(|r| format!("{}-{}", timestamp_ms(r.start_ms), timestamp_ms(r.end_ms)))
                .collect();
            if d.regions.len() > MAX_REGIONS {
                regions.push(format!("… ({} more)", d.regions.len() - MAX_REGIONS));
            }
            vec![
                d.track_id.clone(),
                format!("-{} dB", db(d.reduction_db.abs())),
                d.mode.clone(),
                format!("by {}", d.triggers.join(", ")),
                if regions.is_empty() {
                    "no activity".to_string()
                } else {
                    format!(
                        "{}: {}",
                        plural(d.regions.len(), "region", "regions"),
                        regions.join(", ")
                    )
                },
            ]
        })
        .collect();
    let t = table(
        ctx.out.color(),
        &[
            ("TRACK", Align::Left),
            ("DEPTH", Align::Right),
            ("MODE", Align::Left),
            ("TRIGGERS", Align::Left),
            ("REGIONS", Align::Left),
        ],
        &rows,
    );
    for line in t.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}
