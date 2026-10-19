//! The `soniccraft` command-line interface (docs/DESIGN.md §9.1) and the zero-argument `soniccraft-mcp`
//! launcher.
//!
//! This library target only shares code between the two binaries; it is not a stable API. The
//! user-facing contract is the command line itself (`soniccraft --help`): the subcommands, their flags,
//! the `--json` report shapes (the engine's report types) and the exit statuses
//! (0 success, 1 timeline invalid, 2 usage error, 3 runtime/render failure, 130 interrupted).

#![warn(missing_docs)]

mod assets;
mod cli;
mod commands;
mod context;
mod diag;
mod dmath;
mod exit;
mod format;
mod logging;
mod panic;
mod progress;
mod templates;
mod term;

use std::panic::AssertUnwindSafe;
use std::process::ExitCode;

use crate::cli::{Command, GlobalArgs};
use crate::context::{Ctx, FailureDoc};
use crate::exit::Exit;
use crate::term::Stream;

/// Run `soniccraft` with the process arguments and return its exit status.
pub fn run() -> ExitCode {
    let cli = match cli::parse(std::env::args_os().collect()) {
        Ok(cli) => cli,
        Err(code) => return ExitCode::from(code),
    };
    panic::install_hook(cli.global.verbose > 0);
    let stderr_terminal = Stream::Stderr.is_terminal();
    let ansi = match cli.command {
        // MCP clients capture stderr into log files: escape codes only for a real terminal.
        Command::Mcp(_) => stderr_terminal && cli.global.color.enabled(stderr_terminal),
        _ => cli.global.color.enabled(stderr_terminal),
    };
    logging::init(cli.global.verbose, ansi);
    let ctx = Ctx::new(&cli.global, cli.command.json());
    finish(
        &ctx,
        std::panic::catch_unwind(AssertUnwindSafe(|| commands::execute(&ctx, &cli.command))),
    )
}

/// Run the zero-argument `soniccraft-mcp` launcher: the same server as `soniccraft mcp`, for MCP clients
/// that cannot pass arguments. Configuration comes from the environment (`SONICCRAFT_CACHE_DIR`,
/// `CLAUDE_PROJECT_DIR`, `SONICCRAFT_ALLOW_COMMAND`, `SONICCRAFT_MCP_MAX_RENDERS`, `RUST_LOG`, provider
/// keys).
pub fn run_mcp_launcher() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    if let Some(arg) = args.next() {
        let out = term::Term::new(Stream::Stdout, term::ColorChoice::Never);
        return match arg.to_str() {
            Some("--version" | "-V") => {
                out.line(&format!("soniccraft-mcp {}", env!("CARGO_PKG_VERSION")));
                ExitCode::SUCCESS
            }
            Some("--help" | "-h") => {
                out.line(&format!(
                    "soniccraft-mcp {}\n\nThe SonicCraft MCP server on stdin/stdout (same as `soniccraft mcp`), for MCP\nclients that cannot pass arguments. It takes no arguments; configure it with environment\nvariables: CLAUDE_PROJECT_DIR (base directory for relative paths), SONICCRAFT_CACHE_DIR,\nSONICCRAFT_ALLOW_COMMAND=1 (let timelines run local programs through `command` providers;\noff by default, like `soniccraft mcp --allow-command`), SONICCRAFT_MCP_MAX_RENDERS (renders\nthat run at once, default 2), RUST_LOG (stderr logs) and the provider keys\n(ELEVENLABS_API_KEY, ...).",
                    env!("CARGO_PKG_VERSION")
                ));
                ExitCode::SUCCESS
            }
            _ => {
                let err = term::Term::new(Stream::Stderr, term::ColorChoice::Auto);
                err.line(&format!(
                    "error: soniccraft-mcp takes no arguments (got `{}`)\n  = hint: use `soniccraft mcp --base-dir DIR` to pass options",
                    arg.to_string_lossy()
                ));
                Exit::Usage.into()
            }
        };
    }
    let (allow_command, warning) = commands::mcp::allow_command_from_env();
    if let Some(warning) = warning {
        term::Term::new(Stream::Stderr, term::ColorChoice::Never)
            .line(&format!("warning: {warning}"));
    }
    panic::install_hook(false);
    let stderr_terminal = Stream::Stderr.is_terminal();
    logging::init(0, stderr_terminal && term::ColorChoice::Auto.enabled(true));
    let ctx = Ctx::new(&GlobalArgs::default(), false);
    finish(
        &ctx,
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            commands::mcp::serve(&ctx, None, allow_command)
        })),
    )
}

/// Turn a command outcome into the exit status, reporting failures and panics.
fn finish(ctx: &Ctx, outcome: std::thread::Result<exit::CmdResult>) -> ExitCode {
    match outcome {
        Ok(Ok(exit)) => exit.into(),
        Ok(Err(error)) => ctx.report_failure(&error).into(),
        Err(_) => {
            panic::report();
            if ctx.json {
                let _ = ctx.print_json(&FailureDoc {
                    message: "internal error (see stderr)",
                    diagnostics: &[],
                    hint: None,
                });
            }
            Exit::Failure.into()
        }
    }
}
