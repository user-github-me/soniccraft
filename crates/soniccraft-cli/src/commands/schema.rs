//! `soniccraft schema [-o FILE]`

use crate::cli::SchemaArgs;
use crate::context::Ctx;
use crate::exit::{CliError, CmdResult, Exit};
use crate::term::Style;

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &SchemaArgs) -> CmdResult {
    let schema = soniccraft_engine::timeline_schema();
    let mut text = serde_json::to_string_pretty(&schema)
        .map_err(|e| CliError::failure(format!("cannot serialize the schema: {e}")))?;
    text.push('\n');
    let Some(path) = &args.output else {
        ctx.out.write(&text);
        return Ok(Exit::Success);
    };
    let target = ctx.absolute(path);
    if target.is_dir() {
        return Err(CliError::usage(format!(
            "`-o {}` is a directory; -o takes the schema file",
            path.display()
        )));
    }
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| {
            CliError::failure(format!("cannot create directory `{}`: {e}", dir.display()))
        })?;
    }
    std::fs::write(&target, text.as_bytes())
        .map_err(|e| CliError::failure(format!("cannot write `{}`: {e}", path.display())))?;
    if !ctx.quiet {
        ctx.status(&ctx.err, Style::Success, "wrote", &ctx.show(&target));
    }
    Ok(Exit::Success)
}
