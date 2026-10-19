//! `soniccraft mcp [--base-dir DIR] [--allow-command]` and the zero-argument `soniccraft-mcp` launcher.
//!
//! stdout belongs to the protocol: nothing else is ever written there. Logs (`-v`, `RUST_LOG`) go to
//! stderr without ANSI codes unless stderr is a terminal.
//!
//! `command` providers run local programs, and the timelines of tool calls are data from the client, so
//! the server forbids them unless it is started with `--allow-command` (or `SONICCRAFT_ALLOW_COMMAND=1`,
//! the only way for the launcher, which takes no arguments).

use std::path::PathBuf;
use std::time::Duration;

use soniccraft_engine::{Engine, EngineOptions};
pub(crate) use soniccraft_mcp::ALLOW_COMMAND_ENV;
use soniccraft_mcp::ServerOptions;

use crate::cli::McpArgs;
use crate::context::Ctx;
use crate::exit::{CliError, CmdResult, Exit};

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &McpArgs) -> CmdResult {
    let base_dir = match &args.base_dir {
        Some(dir) => {
            let absolute = ctx.absolute(dir);
            if !absolute.is_dir() {
                return Err(CliError::usage(format!(
                    "--base-dir `{}` is not a directory",
                    dir.display()
                )));
            }
            Some(absolute)
        }
        None => None,
    };
    if !args.allow_command
        && let (_, Some(warning)) = allow_command_from_env()
    {
        ctx.warn(&warning);
    }
    serve(ctx, base_dir, args.allow_command)
}

/// Serve MCP on stdin/stdout until stdin closes. `allow_command` lets timelines use `command` providers.
pub(crate) fn serve(ctx: &Ctx, base_dir: Option<PathBuf>, allow_command: bool) -> CmdResult {
    tracing::debug!(?base_dir, allow_command, "starting the MCP server on stdio");
    // The engine enforces the choice too, whatever the server does with its options.
    let engine = Engine::new(EngineOptions {
        allow_command,
        ..ctx.engine_options(None)
    });
    let runtime = ctx.runtime()?;
    let options = ServerOptions {
        base_dir,
        ..ServerOptions::default()
    }
    .with_allow_command(allow_command);
    let result = runtime.block_on(soniccraft_mcp::serve_stdio(engine, options));
    // Exit promptly once the client has gone: do not wait for an abandoned render.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result.map_err(|e| CliError::failure(format!("MCP server error: {e}")))?;
    Ok(Exit::Success)
}

/// An on/off value: `1`, `true`, `yes`, `on` (the values the MCP server itself accepts for
/// [`ALLOW_COMMAND_ENV`]) and `0`, `false`, `no`, `off` or empty, ignoring case and surrounding
/// whitespace. None for anything else.
pub(crate) fn parse_switch(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "" | "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// clap's parser for the [`ALLOW_COMMAND_ENV`] value of `--allow-command`: anything but an "on" value is
/// off (fail closed; [`allow_command_from_env`] words the warning).
pub(crate) fn switch_value(value: &str) -> Result<bool, std::convert::Infallible> {
    Ok(parse_switch(value).unwrap_or(false))
}

/// Whether [`ALLOW_COMMAND_ENV`] allows `command` providers (unset or unrecognized: no), plus a warning
/// when its value is neither on nor off (e.g. an unexpanded `${SONICCRAFT_ALLOW_COMMAND}` placeholder
/// from an MCP client configuration).
pub(crate) fn allow_command_from_env() -> (bool, Option<String>) {
    let Some(value) = std::env::var_os(ALLOW_COMMAND_ENV) else {
        return (false, None);
    };
    let text = value.to_string_lossy();
    match parse_switch(&text) {
        Some(allow) => (allow, None),
        None => {
            let shown: String = text.chars().take(40).collect();
            (
                false,
                Some(format!(
                    "ignoring {ALLOW_COMMAND_ENV}=`{shown}`: it is not an on/off value, so timelines \
                     cannot run local programs (`command` providers); set it to 1 to allow them, or \
                     unset it"
                )),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert_eq!(parse_switch(on), Some(true), "{on:?}");
            assert_eq!(switch_value(on), Ok(true), "{on:?}");
        }
        for off in ["", "0", "false", "No", "off", "  "] {
            assert_eq!(parse_switch(off), Some(false), "{off:?}");
        }
        for bad in ["2", "y", "maybe", "allow", "${SONICCRAFT_ALLOW_COMMAND}"] {
            assert_eq!(parse_switch(bad), None, "{bad:?}");
            assert_eq!(switch_value(bad), Ok(false), "fails closed: {bad:?}");
        }
    }
}
