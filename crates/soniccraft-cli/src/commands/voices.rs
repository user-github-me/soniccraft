//! `soniccraft voices <PROVIDER> [--json]`

use soniccraft_core::model::ProviderTypeName;
use soniccraft_engine::EngineError;

use crate::cli::VoicesArgs;
use crate::context::Ctx;
use crate::exit::{CliError, CmdResult, Exit};
use crate::format::{Align, plural, table};

/// Descriptions are cut to this many characters in the table (the JSON keeps them whole).
const MAX_DESCRIPTION: usize = 60;

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &VoicesArgs) -> CmdResult {
    let provider = provider_name(&args.provider);
    let engine = ctx.engine();
    let runtime = ctx.runtime()?;
    let report = runtime
        .block_on(engine.voices(provider))
        .map_err(|e| engine_error(provider, e))?;
    if ctx.json {
        ctx.print_json(&report)?;
        return Ok(Exit::Success);
    }
    let term = &ctx.out;
    if report.voices.is_empty() {
        term.line(&format!("{} reports no voices", report.provider));
        return Ok(Exit::Success);
    }
    let dash = |v: &Option<String>| {
        v.clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".into())
    };
    let rows: Vec<Vec<String>> = report
        .voices
        .iter()
        .map(|v| {
            vec![
                v.id.clone(),
                v.name.clone(),
                dash(&v.language),
                dash(&v.gender),
                truncate(&dash(&v.description), MAX_DESCRIPTION),
            ]
        })
        .collect();
    let mut text = table(
        term.color(),
        &[
            ("VOICE ID", Align::Left),
            ("NAME", Align::Left),
            ("LANGUAGE", Align::Left),
            ("GENDER", Align::Left),
            ("DESCRIPTION", Align::Left),
        ],
        &rows,
    );
    if !ctx.quiet {
        text.push_str(&format!(
            "\n{} from {}. Use the VOICE ID as `voice_id` in the timeline.\n",
            plural(report.voices.len(), "voice", "voices"),
            report.provider
        ));
    }
    term.write(&text);
    Ok(Exit::Success)
}

/// The provider type the argument names. Timelines spell types exactly (`elevenlabs`), but a command-line
/// argument may differ in case, separators or surrounding spaces (`ElevenLabs`, `macos-say`), like
/// `--format MP3`.
fn provider_name(arg: &str) -> &str {
    ProviderTypeName::spelling_of(arg).unwrap_or(arg)
}

/// Map an engine error to the CLI's exit statuses: an unknown provider is a usage error (2), a
/// provider or I/O failure a runtime failure (3).
fn engine_error(provider: &str, error: EngineError) -> CliError {
    #[allow(unreachable_patterns)] // the engine may add variants
    match error {
        EngineError::InvalidArgument(message) => {
            CliError::usage(message).with_hint("`soniccraft providers` lists the provider names")
        }
        EngineError::Provider(message) => CliError::failure(message),
        EngineError::Io(e) => {
            CliError::failure(format!("listing the voices of `{provider}` failed: {e}"))
        }
        other => CliError::failure(other.to_string()),
    }
}

/// `s` cut to `max` characters with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn provider_arguments_ignore_case_and_separators() {
        assert_eq!(provider_name("ElevenLabs"), "elevenlabs");
        assert_eq!(provider_name("Eleven-Labs"), "elevenlabs");
        assert_eq!(provider_name(" macOS_Say "), "macos_say");
        assert_eq!(provider_name("11labs"), "11labs");
        assert_eq!(provider_name("mock"), "mock");
        assert_eq!(provider_name("elevnlabs"), "elevnlabs");
    }

    #[test]
    fn error_mapping() {
        let e = engine_error(
            "x",
            EngineError::InvalidArgument("unknown provider `x`".into()),
        );
        assert_eq!(e.exit, Exit::Usage);
        let e = engine_error("x", EngineError::Provider("HTTP 401".into()));
        assert_eq!(e.exit, Exit::Failure);
    }
}
