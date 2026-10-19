//! `soniccraft providers [--json]`

use crate::cli::ProvidersArgs;
use crate::context::Ctx;
use crate::exit::{CmdResult, Exit};
use crate::format::{Align, table};
use crate::term::Style;

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &ProvidersArgs) -> CmdResult {
    let _ = args;
    let report = ctx.engine().providers();
    if ctx.json {
        ctx.print_json(&report)?;
        return Ok(Exit::Success);
    }
    let term = &ctx.out;
    let verbose = ctx.verbose > 0;
    let mut headers = vec![
        ("PROVIDER", Align::Left),
        ("READY", Align::Left),
        ("KIND", Align::Left),
        ("WORD TIMES", Align::Left),
        ("DEFAULT VOICE", Align::Left),
    ];
    if verbose {
        headers.push(("ALIASES", Align::Left));
    }
    headers.push(("STATUS", Align::Left));
    let rows: Vec<Vec<String>> = report
        .providers
        .iter()
        .map(|p| {
            let mut row = vec![
                p.name.clone(),
                if p.available {
                    term.paint(Style::Success, "yes")
                } else {
                    term.paint(Style::Error, "no")
                },
                if p.local { "local" } else { "cloud" }.to_string(),
                if p.word_timestamps {
                    "provider"
                } else {
                    "estimated"
                }
                .to_string(),
                p.default_voice.clone().unwrap_or_else(|| "-".to_string()),
            ];
            if verbose {
                row.push(if p.aliases.is_empty() {
                    "-".to_string()
                } else {
                    p.aliases.join(", ")
                });
            }
            row.push(p.status.clone());
            row
        })
        .collect();
    let mut text = table(term.color(), &headers, &rows);
    if verbose {
        for p in report
            .providers
            .iter()
            .filter(|p| !p.notes.trim().is_empty())
        {
            text.push_str(&format!(
                "\n{} ({}): {}",
                term.paint(Style::Bold, &p.name),
                p.display_name,
                p.notes.trim()
            ));
            if let Some(model) = &p.default_model {
                text.push_str(&format!(" Default model: {model}."));
            }
            if let Some(env) = &p.api_key_env {
                text.push_str(&format!(" Key: ${env}."));
            }
            text.push('\n');
        }
    }
    let ready = report.providers.iter().filter(|p| p.available).count();
    if !ctx.quiet {
        text.push_str(&format!(
            "\n{ready} of {} providers ready here. `soniccraft voices <provider>` lists voice ids{}.\n",
            report.providers.len(),
            if verbose { "" } else { "; -v shows aliases and notes" }
        ));
    }
    term.write(&text);
    Ok(Exit::Success)
}
